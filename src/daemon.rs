use crate::config::{CockpitConfig, CockpitPaths};
use crate::correlate::{build_initial_agent_row, resolve_git_head_file, should_include_terminal};
use crate::herdr::{parse_hc_event_line, FleetTracker, HcRpcSession, HerdrClient};
use crate::model::{
    AgentRow, CockpitSnapshot, FleetSummary, FleetSummaryProjection, InspectResponse, IpcOp,
    IpcRequest, SNAPSHOT_SCHEMA_VERSION,
};
use crate::providers::ProviderCoordinator;
use crate::store::CacheStore;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, RwLock};

pub struct DaemonLock {
    file: File,
}

impl DaemonLock {
    pub fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("failed opening lock file {}", lock_path.display()))?;

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Ok(None);
        }
        Ok(Some(Self { file }))
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Executes a single collection cycle (`cockpit daemon --once` or `cockpit status` fallback).
pub async fn collect_once(
    cfg: &CockpitConfig,
    paths: &CockpitPaths,
    store: &CacheStore,
    herdr: &HerdrClient,
    coord: &ProviderCoordinator,
    include_all: bool,
    force_refresh: bool,
) -> Result<CockpitSnapshot> {
    let mut rpc: Option<HcRpcSession> = None;
    let mut tracker = FleetTracker::new();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_sec = now_ms / 1000;

    let herdr_connected = match herdr.list_terminals_full(&mut rpc).await {
        Ok(terms) => {
            tracker.apply_snapshot(terms, now_ms);
            true
        }
        Err(_) => false,
    };

    let mut snap = materialize_snapshot(
        &mut tracker,
        cfg,
        paths,
        store,
        coord,
        herdr_connected,
        None,
        include_all,
        force_refresh,
        now_ms,
    )
    .await;

    if herdr_connected {
        let proj = FleetSummaryProjection::from_snapshot(&snap);
        let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
    }
    snap.published_at = now_sec;
    Ok(snap)
}

#[allow(clippy::too_many_arguments)]
pub async fn materialize_snapshot(
    tracker: &mut FleetTracker,
    cfg: &CockpitConfig,
    paths: &CockpitPaths,
    store: &CacheStore,
    coord: &ProviderCoordinator,
    herdr_connected: bool,
    daemon_pid: Option<u32>,
    include_all: bool,
    force_refresh: bool,
    now_ms: i64,
) -> CockpitSnapshot {
    let now_sec = now_ms / 1000;
    let ttl_cutoff_ms = now_ms - (cfg.retention.finished_ttl_hours as i64) * 3600 * 1000;
    let blocked_children_map = tracker.blocked_children_map(now_ms);
    let addresses: Vec<String> = tracker.terminals.keys().cloned().collect();

    let mut agents = Vec::new();
    for addr in addresses {
        let Some(term) = tracker.terminals.get(&addr).cloned() else {
            continue;
        };
        if term.state == "finished" {
            if let Some(f_ms) = term.finished_ms {
                if f_ms < ttl_cutoff_ms {
                    continue;
                }
            }
        }
        if !should_include_terminal(&term, include_all) {
            continue;
        }
        let att = tracker.effective_attention(&addr, now_ms);
        let blocked_children = blocked_children_map.get(&addr).cloned().unwrap_or_default();
        let row = build_initial_agent_row(&term, att, blocked_children, now_sec);
        agents.push(row);
    }

    coord
        .enrich_agents(&mut agents, cfg, store, &paths.home_dir, force_refresh)
        .await;
    AgentRow::sort_for_attention_queue(&mut agents);

    let providers = coord.check_all_health(cfg, store, &paths.home_dir).await;
    let summary = FleetSummary::from_agents(&agents);

    CockpitSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        cockpit_version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: now_sec,
        published_at: now_sec,
        daemon_pid,
        herdr_connected,
        summary,
        agents,
        providers,
    }
}

/// Runs the single-instance background daemon and Unix domain socket server.
pub async fn run_daemon(
    cfg: CockpitConfig,
    paths: CockpitPaths,
    custom_socket: Option<PathBuf>,
) -> Result<()> {
    paths.ensure_dirs()?;
    let lock_path = paths.lock_file();
    let Some(_lock_guard) = DaemonLock::try_acquire(&lock_path)? else {
        tracing::info!("Another cockpit daemon instance holds daemon.lock; exiting");
        return Ok(());
    };

    let sock_path = custom_socket.unwrap_or_else(|| paths.socket_file());
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed binding Unix socket at {}", sock_path.display()))?;
    fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600))?;

    let store = CacheStore::open(&paths.db_file())?;
    let herdr = HerdrClient::default();
    let coord = ProviderCoordinator::new();
    let mut rpc: Option<HcRpcSession> = None;
    let mut tracker = FleetTracker::new();
    let pid = std::process::id();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let herdr_connected = match herdr.list_terminals_full(&mut rpc).await {
        Ok(terms) => {
            tracker.apply_snapshot(terms, now_ms);
            true
        }
        Err(_) => false,
    };

    let initial_snap = materialize_snapshot(
        &mut tracker,
        &cfg,
        &paths,
        &store,
        &coord,
        herdr_connected,
        Some(pid),
        false,
        false,
        now_ms,
    )
    .await;

    if herdr_connected {
        let proj = FleetSummaryProjection::from_snapshot(&initial_snap);
        let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
    }

    let shared_snap = Arc::new(RwLock::new(initial_snap));
    let (tx, _) = broadcast::channel::<CockpitSnapshot>(32);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<bool>(8);

    // Spawn hc events --json consumer if available
    let mut event_child_and_reader = herdr.spawn_events_stream().ok();
    let mut event_line = String::new();
    let mut git_head_cache: BTreeMap<String, String> = BTreeMap::new();

    let mut worktree_tick = tokio::time::interval(Duration::from_secs(2));
    let mut reconcile_tick = tokio::time::interval(Duration::from_secs(90));

    loop {
        let has_event_stream = event_child_and_reader.is_some();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            conn_res = listener.accept() => {
                if let Ok((stream, _)) = conn_res {
                    let snap_clone = Arc::clone(&shared_snap);
                    let sub_rx = tx.subscribe();
                    let r_tx = refresh_tx.clone();
                    let cfg_c = cfg.clone();
                    let paths_c = paths.clone();
                    let store_c = store.clone();
                    let herdr_c = herdr.clone();
                    let coord_c = coord.clone();
                    tokio::spawn(async move {
                        let _ = handle_ipc_client(
                            stream,
                            snap_clone,
                            sub_rx,
                            r_tx,
                            cfg_c,
                            paths_c,
                            store_c,
                            herdr_c,
                            coord_c,
                        ).await;
                    });
                }
            }
            Some(force) = refresh_rx.recv() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                if let Ok(terms) = herdr.list_terminals_full(&mut rpc).await {
                    tracker.apply_snapshot(terms, ts_ms);
                }
                let updated = materialize_snapshot(
                    &mut tracker,
                    &cfg,
                    &paths,
                    &store,
                    &coord,
                    true,
                    Some(pid),
                    false,
                    force,
                    ts_ms,
                ).await;
                let proj = FleetSummaryProjection::from_snapshot(&updated);
                let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
                *shared_snap.write().await = updated.clone();
                let _ = tx.send(updated);
            }
            line_res = async {
                if let Some((_, ref mut reader)) = event_child_and_reader {
                    event_line.clear();
                    reader.read_line(&mut event_line).await
                } else {
                    std::future::pending().await
                }
            }, if has_event_stream => {
                match line_res {
                    Ok(0) | Err(_) => {
                        event_child_and_reader = None;
                    }
                    Ok(_) => {
                        let ts_ms = chrono::Utc::now().timestamp_millis();
                        if let Ok(item) = parse_hc_event_line(event_line.trim()) {
                            if tracker.apply_stream_item(item, ts_ms) {
                                let updated = materialize_snapshot(
                                    &mut tracker,
                                    &cfg,
                                    &paths,
                                    &store,
                                    &coord,
                                    true,
                                    Some(pid),
                                    false,
                                    false,
                                    ts_ms,
                                ).await;
                                let proj = FleetSummaryProjection::from_snapshot(&updated);
                                let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
                                *shared_snap.write().await = updated.clone();
                                let _ = tx.send(updated);
                            }
                        }
                    }
                }
            }
            _ = worktree_tick.tick() => {
                // Poll resolved <gitdir>/HEAD for live agent directories
                let mut head_changed = false;
                for (addr, term) in &tracker.terminals {
                    if term.state == "finished" {
                        continue;
                    }
                    let cwd = term.effective_cwd();
                    if let Some(head_file) = resolve_git_head_file(&cwd) {
                        if let Ok(contents) = fs::read_to_string(&head_file) {
                            let prev = git_head_cache.insert(addr.clone(), contents.clone());
                            if prev.is_some() && prev.as_deref() != Some(contents.as_str()) {
                                head_changed = true;
                            }
                        }
                    }
                }
                if head_changed {
                    let ts_ms = chrono::Utc::now().timestamp_millis();
                    let updated = materialize_snapshot(
                        &mut tracker,
                        &cfg,
                        &paths,
                        &store,
                        &coord,
                        rpc.is_some(),
                        Some(pid),
                        false,
                        false,
                        ts_ms,
                    ).await;
                    let proj = FleetSummaryProjection::from_snapshot(&updated);
                    let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
                    *shared_snap.write().await = updated.clone();
                    let _ = tx.send(updated);
                }
            }
            _ = reconcile_tick.tick() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                let _ = store.prune_expired(ts_ms / 1000, cfg.retention.finished_ttl_hours);
                if event_child_and_reader.is_none() {
                    event_child_and_reader = herdr.spawn_events_stream().ok();
                }
                if let Ok(terms) = herdr.list_terminals_full(&mut rpc).await {
                    tracker.apply_snapshot(terms, ts_ms);
                    let updated = materialize_snapshot(
                        &mut tracker,
                        &cfg,
                        &paths,
                        &store,
                        &coord,
                        true,
                        Some(pid),
                        false,
                        false,
                        ts_ms,
                    ).await;
                    let proj = FleetSummaryProjection::from_snapshot(&updated);
                    let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
                    *shared_snap.write().await = updated.clone();
                    let _ = tx.send(updated);
                }
            }
        }
    }

    // Graceful shutdown: clear daemon_pid in doc.set and remove socket
    let mut final_snap = shared_snap.read().await.clone();
    final_snap.daemon_pid = None;
    let proj = FleetSummaryProjection::from_snapshot(&final_snap);
    let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
    let _ = fs::remove_file(&sock_path);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_client(
    stream: UnixStream,
    shared_snap: Arc<RwLock<CockpitSnapshot>>,
    mut sub_rx: broadcast::Receiver<CockpitSnapshot>,
    refresh_tx: tokio::sync::mpsc::Sender<bool>,
    cfg: CockpitConfig,
    paths: CockpitPaths,
    store: CacheStore,
    herdr: HerdrClient,
    coord: ProviderCoordinator,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut req_line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut req_line)).await??;
    if n == 0 {
        return Ok(());
    }
    let req: IpcRequest = serde_json::from_str(req_line.trim())?;

    match req.op {
        IpcOp::Status => {
            let snap = if req.include_all {
                collect_once(&cfg, &paths, &store, &herdr, &coord, true, false).await?
            } else {
                shared_snap.read().await.clone()
            };
            let mut out = serde_json::to_string(&snap)?;
            out.push('\n');
            write_half.write_all(out.as_bytes()).await?;
        }
        IpcOp::Refresh => {
            let _ = refresh_tx.send(true).await;
            tokio::time::sleep(Duration::from_millis(120)).await;
            let snap = shared_snap.read().await.clone();
            let mut out = serde_json::to_string(&snap)?;
            out.push('\n');
            write_half.write_all(out.as_bytes()).await?;
        }
        IpcOp::Inspect => {
            let target = req.terminal.unwrap_or_default();
            let snap = shared_snap.read().await.clone();
            let found = snap
                .agents
                .iter()
                .find(|a| a.address == target || a.name == target)
                .cloned();
            let agent = match found {
                Some(a) => a,
                None => {
                    let all_snap =
                        collect_once(&cfg, &paths, &store, &herdr, &coord, true, false).await?;
                    all_snap
                        .agents
                        .into_iter()
                        .find(|a| a.address == target || a.name == target)
                        .with_context(|| format!("terminal '{}' not found", target))?
                }
            };
            let resp = InspectResponse {
                agent,
                providers: snap.providers,
            };
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            write_half.write_all(out.as_bytes()).await?;
        }
        IpcOp::Watch => {
            let initial = shared_snap.read().await.clone();
            let mut line = serde_json::to_string(&initial)?;
            line.push('\n');
            write_half.write_all(line.as_bytes()).await?;

            while let Ok(next_snap) = sub_rx.recv().await {
                let mut l = serde_json::to_string(&next_snap)?;
                l.push('\n');
                if write_half.write_all(l.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

pub async fn query_socket_once<T: serde::de::DeserializeOwned>(
    sock_path: &Path,
    req: &IpcRequest,
) -> Result<T> {
    let stream = tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(sock_path))
        .await
        .context("socket connect timeout")??;
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut resp_line))
        .await
        .context("socket read timeout")??;
    if n == 0 {
        bail!("empty response from cockpit.sock");
    }
    Ok(serde_json::from_str(resp_line.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_lock_prevents_second_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("daemon.lock");
        let lock1 = DaemonLock::try_acquire(&lock_file).unwrap();
        assert!(lock1.is_some());
        let lock2 = DaemonLock::try_acquire(&lock_file).unwrap();
        assert!(lock2.is_none());
        drop(lock1);
        let lock3 = DaemonLock::try_acquire(&lock_file).unwrap();
        assert!(lock3.is_some());
    }
}
