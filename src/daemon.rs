use crate::config::{CockpitConfig, CockpitPaths};
use crate::correlate::{build_initial_agent_row, resolve_git_head_file, should_include_terminal};
use crate::herdr::{parse_hc_event_line, FleetTracker, HcRpcSession, HerdrClient};
use crate::model::{
    AgentRow, CockpitSnapshot, FleetSummary, FleetSummaryProjection, InspectResponse, IpcOp,
    IpcRequest, SNAPSHOT_SCHEMA_VERSION,
};
use crate::orch::delivery::{DeliveryScheduler, ScheduleOutcome};
use crate::orch::reconciler::OrchReconciler;
use crate::orch::store::OrchStore;
use crate::providers::ProviderCoordinator;
use crate::store::CacheStore;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DAEMON_START_MAX_ATTEMPTS: usize = 20;
const MAX_DRAIN_EVENT_LINES: usize = 64;
const IPC_REFRESH_WAIT_TIMEOUT: Duration = Duration::from_secs(4);

struct RefreshRequest {
    force: bool,
    reply: oneshot::Sender<CockpitSnapshot>,
}

struct DaemonPublishCtx<'a> {
    cfg: &'a CockpitConfig,
    paths: &'a CockpitPaths,
    store: &'a CacheStore,
    coord: &'a ProviderCoordinator,
    shared_snap: &'a Arc<RwLock<CockpitSnapshot>>,
    tx: &'a broadcast::Sender<CockpitSnapshot>,
    pid: u32,
}

pub async fn ensure_daemon_running(paths: &CockpitPaths) -> Result<()> {
    let exe = std::env::current_exe().context("failed resolving current tchelvis executable")?;
    ensure_daemon_running_with_exe(
        paths,
        &exe,
        DAEMON_START_POLL_INTERVAL,
        DAEMON_START_MAX_ATTEMPTS,
    )
    .await
}

async fn ensure_daemon_running_with_exe(
    paths: &CockpitPaths,
    exe: &Path,
    poll_interval: Duration,
    max_attempts: usize,
) -> Result<()> {
    let sock_path = paths.socket_file();
    if UnixStream::connect(&sock_path).await.is_ok() {
        return Ok(());
    }
    let mut child = Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed spawning daemon process `{}`", exe.display()))?;

    let mut last_connect_err = None;
    for _ in 0..max_attempts.max(1) {
        tokio::time::sleep(poll_interval).await;
        match UnixStream::connect(&sock_path).await {
            Ok(_) => return Ok(()),
            Err(err) => last_connect_err = Some(err),
        }
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                bail!(
                    "daemon process `{}` exited with status {} before socket `{}` accepted connections",
                    exe.display(),
                    status,
                    sock_path.display()
                );
            }
        }
    }

    let err_detail = last_connect_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "socket never became connectable".to_string());
    bail!(
        "timed out waiting for daemon socket `{}` to accept connections (exe `{}`): {}",
        sock_path.display(),
        exe.display(),
        err_detail
    )
}

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

/// Executes a single collection cycle (`tchelvis daemon --once` or `tchelvis status` fallback).
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
    let active_orch_children = OrchStore::open(&paths.orch_db_file())
        .and_then(|s| s.active_dispatches_by_parent())
        .unwrap_or_default();
    let active_children_map = tracker.active_children_map(&active_orch_children, now_ms);
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
        let mut row = build_initial_agent_row(&term, att, blocked_children, now_sec);
        row.active_children = active_children_map.get(&addr).cloned().unwrap_or_default();
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

async fn refresh_and_publish_snapshot(
    ctx: &DaemonPublishCtx<'_>,
    tracker: &mut FleetTracker,
    herdr: &HerdrClient,
    rpc: &mut Option<HcRpcSession>,
    herdr_connected: bool,
    force_refresh: bool,
    ts_ms: i64,
) -> CockpitSnapshot {
    let updated = materialize_snapshot(
        tracker,
        ctx.cfg,
        ctx.paths,
        ctx.store,
        ctx.coord,
        herdr_connected,
        Some(ctx.pid),
        false,
        force_refresh,
        ts_ms,
    )
    .await;
    if herdr_connected {
        let proj = FleetSummaryProjection::from_snapshot(&updated);
        if let Err(err) = herdr.publish_doc_snapshot(rpc, &proj).await {
            tracing::warn!("failed publishing doc snapshot to herdr: {err:#}");
            *rpc = None;
        }
    }
    *ctx.shared_snap.write().await = updated.clone();
    let _ = ctx.tx.send(updated.clone());
    updated
}

async fn run_orch_cycle(
    orch_store: &OrchStore,
    herdr: &HerdrClient,
    delivery_scheduler: &mut DeliveryScheduler,
    tracker: &FleetTracker,
    ts_ms: i64,
    source: &str,
) -> (crate::orch::ReconcileOutcome, ScheduleOutcome) {
    let reconcile_outcome = match OrchReconciler::reconcile_active_dispatches(
        orch_store, herdr, tracker, ts_ms,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!("orch reconcile failed ({source}): {err:#}");
            Default::default()
        }
    };
    let schedule_outcome = match delivery_scheduler.schedule_pending(orch_store, tracker, ts_ms) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!("orch pointer scheduling failed ({source}): {err:#}");
            Default::default()
        }
    };
    (reconcile_outcome, schedule_outcome)
}

async fn read_event_line_cancel_safe<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    loop {
        let n = reader.read_until(b'\n', buf).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            let raw = std::mem::take(buf);
            return Ok(Some(String::from_utf8_lossy(&raw).into_owned()));
        }
        if buf.ends_with(b"\n") {
            let raw = std::mem::take(buf);
            return Ok(Some(String::from_utf8_lossy(&raw).into_owned()));
        }
    }
}

async fn drain_ready_event_lines<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    event_buf: &mut Vec<u8>,
    tracker: &mut FleetTracker,
    herdr_connected: &mut bool,
    ts_ms: i64,
    max_drain: usize,
) -> (bool, bool) {
    let mut any_changed = false;
    let mut stream_closed = false;
    for _ in 0..max_drain {
        tokio::select! {
            biased;
            next_res = read_event_line_cancel_safe(reader, event_buf) => {
                match next_res {
                    Ok(Some(next_line)) => {
                        if let Ok(item) = parse_hc_event_line(next_line.trim()) {
                            *herdr_connected = true;
                            if tracker.apply_stream_item(item, ts_ms) {
                                any_changed = true;
                            }
                        }
                    }
                    Ok(None) => {
                        stream_closed = true;
                        break;
                    }
                    Err(err) => {
                        tracing::warn!("hc events stream read error: {err}");
                        stream_closed = true;
                        break;
                    }
                }
            }
            _ = std::future::ready(()) => break,
        }
    }
    (any_changed, stream_closed)
}

async fn kill_and_reap_events_child(
    event_child_and_reader: &mut Option<(
        tokio::process::Child,
        BufReader<tokio::process::ChildStdout>,
    )>,
) {
    if let Some((mut child, reader)) = event_child_and_reader.take() {
        drop(reader);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
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
        tracing::info!("Another tchelvis daemon instance holds daemon.lock; exiting");
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
    let orch_store = OrchStore::open(&paths.orch_db_file())?;
    let herdr = HerdrClient::default();
    let coord = ProviderCoordinator::new();
    let mut rpc: Option<HcRpcSession> = None;
    let mut tracker = FleetTracker::new();
    let pid = std::process::id();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut herdr_connected = match herdr.list_terminals_full(&mut rpc).await {
        Ok(terms) => {
            tracker.apply_snapshot(terms, now_ms);
            true
        }
        Err(_) => {
            rpc = None;
            false
        }
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
        if let Err(err) = herdr.publish_doc_snapshot(&mut rpc, &proj).await {
            tracing::warn!("failed publishing initial doc snapshot to herdr: {err:#}");
            rpc = None;
        }
    }

    let shared_snap = Arc::new(RwLock::new(initial_snap));
    let (tx, _) = broadcast::channel::<CockpitSnapshot>(32);
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<RefreshRequest>(8);
    let (orch_wake_tx, mut orch_wake_rx) = mpsc::channel::<()>(32);

    let pub_ctx = DaemonPublishCtx {
        cfg: &cfg,
        paths: &paths,
        store: &store,
        coord: &coord,
        shared_snap: &shared_snap,
        tx: &tx,
        pid,
    };

    // Spawn hc events --json consumer if available
    let mut event_child_and_reader = herdr.spawn_events_stream().ok();
    let mut event_buf: Vec<u8> = Vec::new();
    let mut git_head_cache: BTreeMap<String, String> = BTreeMap::new();

    let mut orch_tick = tokio::time::interval(Duration::from_millis(500));
    let mut worktree_tick = tokio::time::interval(Duration::from_secs(2));
    let mut reconcile_tick = tokio::time::interval(Duration::from_secs(5));
    let mut delivery_scheduler = DeliveryScheduler::new(herdr.clone());

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
                    let o_tx = orch_wake_tx.clone();
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
                            o_tx,
                            cfg_c,
                            paths_c,
                            store_c,
                            herdr_c,
                            coord_c,
                        ).await;
                    });
                }
            }
            Some(req) = refresh_rx.recv() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                match herdr.list_terminals_full(&mut rpc).await {
                    Ok(terms) => {
                        tracker.apply_snapshot(terms, ts_ms);
                        herdr_connected = true;
                    }
                    Err(err) => {
                        tracing::warn!("herdr list_terminals_full failed during refresh: {err:#}");
                        rpc = None;
                        herdr_connected = false;
                    }
                }
                let updated = refresh_and_publish_snapshot(
                    &pub_ctx,
                    &mut tracker,
                    &herdr,
                    &mut rpc,
                    herdr_connected,
                    req.force,
                    ts_ms,
                )
                .await;
                let _ = req.reply.send(updated);
            }
            Some(()) = orch_wake_rx.recv() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                if herdr_connected {
                    if let Err(err) = crate::loops::watch::watch_loops(
                        &orch_store,
                        &tracker,
                        &crate::loops::watch::LinuxProcTable,
                        ts_ms,
                    ) {
                        tracing::warn!("loop watch failed (wake): {err:#}");
                    }
                }
                let (reconcile_outcome, schedule_outcome) = run_orch_cycle(
                    &orch_store,
                    &herdr,
                    &mut delivery_scheduler,
                    &tracker,
                    ts_ms,
                    "wake",
                )
                .await;
                if reconcile_outcome.state_changed || schedule_outcome.stalled_count > 0 {
                    let _ = refresh_and_publish_snapshot(
                        &pub_ctx,
                        &mut tracker,
                        &herdr,
                        &mut rpc,
                        herdr_connected,
                        false,
                        ts_ms,
                    )
                    .await;
                }
            }
            Some(completed) = delivery_scheduler.recv_completed() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                match delivery_scheduler.handle_completed(&orch_store, completed, ts_ms) {
                    Ok(true) => {
                        let _ = delivery_scheduler.schedule_pending(&orch_store, &tracker, ts_ms);
                        let _ = refresh_and_publish_snapshot(
                            &pub_ctx,
                            &mut tracker,
                            &herdr,
                            &mut rpc,
                            herdr_connected,
                            false,
                            ts_ms,
                        )
                        .await;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        tracing::warn!("orch pointer completion update failed: {err:#}");
                    }
                }
            }
            line_res = async {
                if let Some((_, ref mut reader)) = event_child_and_reader {
                    read_event_line_cancel_safe(reader, &mut event_buf).await
                } else {
                    std::future::pending().await
                }
            }, if has_event_stream => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                let mut any_changed = false;
                let mut stream_closed = false;

                match line_res {
                    Ok(Some(line)) => {
                        if let Ok(item) = parse_hc_event_line(line.trim()) {
                            herdr_connected = true;
                            if tracker.apply_stream_item(item, ts_ms) {
                                any_changed = true;
                            }
                        }
                        if let Some((_, ref mut reader)) = event_child_and_reader {
                            let (drained_changed, drained_closed) = drain_ready_event_lines(
                                reader,
                                &mut event_buf,
                                &mut tracker,
                                &mut herdr_connected,
                                ts_ms,
                                MAX_DRAIN_EVENT_LINES,
                            )
                            .await;
                            any_changed |= drained_changed;
                            stream_closed |= drained_closed;
                        }
                    }
                    Ok(None) => {
                        stream_closed = true;
                    }
                    Err(err) => {
                        tracing::warn!("hc events stream read error: {err}");
                        stream_closed = true;
                    }
                }

                if stream_closed {
                    kill_and_reap_events_child(&mut event_child_and_reader).await;
                    event_buf.clear();
                    if rpc.is_none() {
                        herdr_connected = false;
                    }
                }

                if any_changed {
                    let _ = run_orch_cycle(
                        &orch_store,
                        &herdr,
                        &mut delivery_scheduler,
                        &tracker,
                        ts_ms,
                        "event",
                    )
                    .await;
                    let _ = refresh_and_publish_snapshot(
                        &pub_ctx,
                        &mut tracker,
                        &herdr,
                        &mut rpc,
                        herdr_connected,
                        false,
                        ts_ms,
                    )
                    .await;
                }
            }
            _ = orch_tick.tick() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                let (reconcile_outcome, schedule_outcome) = run_orch_cycle(
                    &orch_store,
                    &herdr,
                    &mut delivery_scheduler,
                    &tracker,
                    ts_ms,
                    "tick",
                )
                .await;
                if reconcile_outcome.state_changed || schedule_outcome.stalled_count > 0 {
                    let _ = refresh_and_publish_snapshot(
                        &pub_ctx,
                        &mut tracker,
                        &herdr,
                        &mut rpc,
                        herdr_connected,
                        false,
                        ts_ms,
                    )
                    .await;
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
                    let _ = refresh_and_publish_snapshot(
                        &pub_ctx,
                        &mut tracker,
                        &herdr,
                        &mut rpc,
                        herdr_connected,
                        false,
                        ts_ms,
                    )
                    .await;
                }
            }
            _ = reconcile_tick.tick() => {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                let _ = store.prune_expired(ts_ms / 1000, cfg.retention.finished_ttl_hours);
                if event_child_and_reader.is_none() {
                    event_buf.clear();
                    event_child_and_reader = herdr.spawn_events_stream().ok();
                }
                let prev_connected = herdr_connected;
                match herdr.list_terminals_full(&mut rpc).await {
                    Ok(terms) => {
                        herdr_connected = true;
                        tracker.apply_snapshot(terms, ts_ms);
                        if let Err(err) = crate::loops::watch::watch_loops(
                            &orch_store,
                            &tracker,
                            &crate::loops::watch::LinuxProcTable,
                            ts_ms,
                        ) {
                            tracing::warn!("loop watch failed (reconcile): {err:#}");
                        }
                        let _ = run_orch_cycle(
                            &orch_store,
                            &herdr,
                            &mut delivery_scheduler,
                            &tracker,
                            ts_ms,
                            "reconcile",
                        )
                        .await;
                        let _ = refresh_and_publish_snapshot(
                            &pub_ctx,
                            &mut tracker,
                            &herdr,
                            &mut rpc,
                            herdr_connected,
                            false,
                            ts_ms,
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::warn!("herdr list_terminals_full failed during reconcile: {err:#}");
                        rpc = None;
                        herdr_connected = false;
                        if prev_connected {
                            let _ = refresh_and_publish_snapshot(
                                &pub_ctx,
                                &mut tracker,
                                &herdr,
                                &mut rpc,
                                herdr_connected,
                                false,
                                ts_ms,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    // Graceful shutdown: reap event stream child, clear daemon_pid in doc.set, and remove socket
    kill_and_reap_events_child(&mut event_child_and_reader).await;
    if herdr_connected {
        let mut final_snap = shared_snap.read().await.clone();
        final_snap.daemon_pid = None;
        let proj = FleetSummaryProjection::from_snapshot(&final_snap);
        let _ = herdr.publish_doc_snapshot(&mut rpc, &proj).await;
    }
    let _ = fs::remove_file(&sock_path);
    Ok(())
}

async fn request_refresh_and_wait(
    refresh_tx: &mpsc::Sender<RefreshRequest>,
    shared_snap: &Arc<RwLock<CockpitSnapshot>>,
    force: bool,
    timeout: Duration,
) -> CockpitSnapshot {
    let (reply_tx, reply_rx) = oneshot::channel();
    let req = RefreshRequest {
        force,
        reply: reply_tx,
    };
    if refresh_tx.send(req).await.is_ok() {
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(snap)) => return snap,
            Ok(Err(_)) => {
                tracing::warn!("refresh reply channel dropped before completing");
            }
            Err(_) => {
                tracing::warn!("timed out waiting for daemon refresh cycle after {timeout:?}");
            }
        }
    }
    shared_snap.read().await.clone()
}

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_client(
    stream: UnixStream,
    shared_snap: Arc<RwLock<CockpitSnapshot>>,
    sub_rx: broadcast::Receiver<CockpitSnapshot>,
    refresh_tx: mpsc::Sender<RefreshRequest>,
    orch_wake_tx: mpsc::Sender<()>,
    cfg: CockpitConfig,
    paths: CockpitPaths,
    store: CacheStore,
    herdr: HerdrClient,
    coord: ProviderCoordinator,
) -> Result<()> {
    handle_ipc_client_with_timeout(
        stream,
        shared_snap,
        sub_rx,
        refresh_tx,
        orch_wake_tx,
        cfg,
        paths,
        store,
        herdr,
        coord,
        IPC_REFRESH_WAIT_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_client_with_timeout(
    stream: UnixStream,
    shared_snap: Arc<RwLock<CockpitSnapshot>>,
    mut sub_rx: broadcast::Receiver<CockpitSnapshot>,
    refresh_tx: mpsc::Sender<RefreshRequest>,
    orch_wake_tx: mpsc::Sender<()>,
    cfg: CockpitConfig,
    paths: CockpitPaths,
    store: CacheStore,
    herdr: HerdrClient,
    coord: ProviderCoordinator,
    refresh_timeout: Duration,
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
                let stale =
                    chrono::Utc::now().timestamp() - shared_snap.read().await.generated_at >= 2;
                if stale {
                    request_refresh_and_wait(&refresh_tx, &shared_snap, false, refresh_timeout)
                        .await
                } else {
                    shared_snap.read().await.clone()
                }
            };
            let mut out = serde_json::to_string(&snap)?;
            out.push('\n');
            write_half.write_all(out.as_bytes()).await?;
        }
        IpcOp::Refresh => {
            let snap =
                request_refresh_and_wait(&refresh_tx, &shared_snap, true, refresh_timeout).await;
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
        IpcOp::OrchWake => {
            let _ = orch_wake_tx.try_send(());
            write_half.write_all(b"{\"ok\":true}\n").await?;
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
        bail!("empty response from tchelvis.sock");
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

    fn make_test_paths(root: &Path) -> CockpitPaths {
        let paths = CockpitPaths {
            home_dir: root.to_path_buf(),
            config_dir: root.join("cfg"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        };
        paths.ensure_dirs().unwrap();
        paths
    }

    #[tokio::test]
    async fn test_materialize_snapshot_active_children_uses_ms_not_sec() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(tmp.path());
        let cfg = CockpitConfig::default();
        let store = CacheStore::open(&paths.db_file()).unwrap();
        let coord = ProviderCoordinator::new();

        let mut tracker = FleetTracker::new();
        let agent_fg = Some(crate::herdr::HcForeground {
            name: Some("claude".into()),
            argv: vec!["claude".into()],
            ..Default::default()
        });
        let parent = crate::herdr::HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            reported: true,
            foreground: agent_fg.clone(),
            status: Some(crate::herdr::HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let child = crate::herdr::HcTerminal {
            address: "t2".into(),
            parent: Some("t1".into()),
            state: "live".into(),
            reported: true,
            foreground: agent_fg,
            status: Some(crate::herdr::HcStatus {
                state: "blocked".into(),
                kind: Some("permission".into()),
                msg: Some("Confirm?".into()),
                app: None,
            }),
            status_ms: Some(10_000),
            turn: 1,
            ..Default::default()
        };
        tracker.apply_snapshot(vec![parent, child], 10_000);

        let clear_ev = parse_hc_event_line(
            r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"idle"},"seq":2,"terminal":"t2","type":"status"}}"#,
        )
        .unwrap();
        tracker.apply_stream_item(clear_ev, 10_200);

        // At 11_800 ms (1600 ms after clear at 10_200 ms), hysteresis has expired.
        let snap = materialize_snapshot(
            &mut tracker,
            &cfg,
            &paths,
            &store,
            &coord,
            true,
            Some(1),
            false,
            false,
            11_800,
        )
        .await;
        let t1 = snap.agents.iter().find(|a| a.address == "t1").unwrap();
        assert!(t1.blocked_children.is_empty());
        assert!(
            t1.active_children.is_empty(),
            "active_children should clear on ms boundary, got: {:?}",
            t1.active_children
        );
    }

    #[tokio::test]
    async fn test_event_stream_partial_line_across_cancelled_poll() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let mut buf_reader = BufReader::new(reader);
        let mut event_buf: Vec<u8> = Vec::new();

        // Write first chunk without newline.
        writer
            .write_all(
                br#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"working"},"#,
            )
            .await
            .unwrap();

        // Poll read_event_line_cancel_safe inside select! and cancel it when another branch wins.
        let cancelled = tokio::select! {
            biased;
            res = read_event_line_cancel_safe(&mut buf_reader, &mut event_buf) => {
                panic!("partial line should not resolve before newline, got {res:?}");
            }
            _ = std::future::ready(()) => true,
        };
        assert!(cancelled);
        assert!(
            !event_buf.is_empty(),
            "partial buffer must be retained across cancelled polls"
        );

        // Write second chunk completing the line, plus a second back-to-back line, then close.
        writer
            .write_all(
                br#""seq":7,"terminal":"t1","type":"status"}}
{"jsonrpc":"2.0","method":"heartbeat","params":{}}
"#,
            )
            .await
            .unwrap();
        drop(writer);

        let line1 = read_event_line_cancel_safe(&mut buf_reader, &mut event_buf)
            .await
            .unwrap()
            .expect("expected first completed line");
        assert!(event_buf.is_empty());
        let item1 = parse_hc_event_line(line1.trim()).unwrap();
        assert!(matches!(
            item1,
            crate::herdr::HcStreamItem::Event {
                seq: 7,
                terminal: Some(_),
                ..
            }
        ));

        let line2 = read_event_line_cancel_safe(&mut buf_reader, &mut event_buf)
            .await
            .unwrap()
            .expect("expected second line");
        assert_eq!(
            parse_hc_event_line(line2.trim()).unwrap(),
            crate::herdr::HcStreamItem::Heartbeat
        );

        assert!(read_event_line_cancel_safe(&mut buf_reader, &mut event_buf)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_ensure_daemon_running_errors_when_socket_never_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(tmp.path());

        let fake_exe = tmp.path().join("fake-tchelvis");
        crate::test_support::write_executable(&fake_exe, "#!/usr/bin/env bash\nexit 0\n");

        let err = ensure_daemon_running_with_exe(&paths, &fake_exe, Duration::from_millis(2), 3)
            .await
            .expect_err("expected error when daemon socket never appears");
        assert!(
            err.to_string()
                .contains("timed out waiting for daemon socket"),
            "unexpected error message: {err}"
        );

        // Also verify a stale non-listening socket file on disk does NOT falsely return Ok.
        // Bind SOCK_STREAM without listen() so a concurrent fork() in another test thread
        // inheriting the fd before exec() can never make UnixStream::connect() succeed.
        let sock_path = paths.socket_file();
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let c_path = CString::new(sock_path.as_os_str().as_bytes()).unwrap();
            let bytes = c_path.as_bytes_with_nul();
            // SAFETY: zeroed sockaddr_un is valid; sun_path bounds are checked below.
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            assert!(
                bytes.len() <= addr.sun_path.len(),
                "socket path too long: {}",
                sock_path.display()
            );
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (dst, &src) in addr.sun_path.iter_mut().zip(bytes.iter()) {
                *dst = src as libc::c_char;
            }
            // SAFETY: valid AF_UNIX/SOCK_STREAM socket args and sockaddr_un pointer/length.
            let fd =
                unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            assert!(
                fd >= 0,
                "socket() failed: {}",
                std::io::Error::last_os_error()
            );
            let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len())
                as libc::socklen_t;
            let rc = unsafe {
                libc::bind(
                    fd,
                    (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                    len,
                )
            };
            let bind_err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            assert_eq!(rc, 0, "bind() failed: {bind_err}");
        }
        assert!(sock_path.exists());
        let err_stale =
            ensure_daemon_running_with_exe(&paths, &fake_exe, Duration::from_millis(2), 3)
                .await
                .expect_err("stale non-accepting socket file must still fail");
        assert!(err_stale
            .to_string()
            .contains("timed out waiting for daemon socket"));
    }

    #[tokio::test]
    async fn test_ensure_daemon_running_succeeds_when_socket_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(tmp.path());
        let sock_path = paths.socket_file();
        let listener = UnixListener::bind(&sock_path).unwrap();
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let missing_exe = tmp.path().join("should-not-be-spawned");
        ensure_daemon_running_with_exe(&paths, &missing_exe, Duration::from_millis(2), 3)
            .await
            .unwrap();
        accept_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_ipc_status_and_refresh_wait_for_fresh_snapshot_and_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(tmp.path());
        let cfg = CockpitConfig::default();
        let store = CacheStore::open(&paths.db_file()).unwrap();
        let herdr = HerdrClient::new("/nonexistent-hc");
        let coord = ProviderCoordinator::new();

        let stale_snap = CockpitSnapshot {
            generated_at: 0,
            published_at: 0,
            daemon_pid: Some(100),
            ..Default::default()
        };
        let shared_snap = Arc::new(RwLock::new(stale_snap));
        let (tx, _) = broadcast::channel::<CockpitSnapshot>(8);
        let (refresh_tx, mut refresh_rx) = mpsc::channel::<RefreshRequest>(8);
        let (orch_wake_tx, _) = mpsc::channel::<()>(8);

        let got_req = Arc::new(tokio::sync::Notify::new());
        let allow_reply = Arc::new(tokio::sync::Notify::new());
        let got_req_w = Arc::clone(&got_req);
        let allow_reply_w = Arc::clone(&allow_reply);
        let shared_clone = Arc::clone(&shared_snap);

        let refresh_worker = tokio::spawn(async move {
            while let Some(req) = refresh_rx.recv().await {
                got_req_w.notify_one();
                allow_reply_w.notified().await;
                let fresh = CockpitSnapshot {
                    generated_at: chrono::Utc::now().timestamp(),
                    published_at: chrono::Utc::now().timestamp(),
                    daemon_pid: Some(if req.force { 202 } else { 201 }),
                    herdr_connected: true,
                    ..Default::default()
                };
                *shared_clone.write().await = fresh.clone();
                let _ = req.reply.send(fresh);
            }
        });

        // 1. Stale IpcOp::Status must block until refresh completes and receive pid=201.
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let srv_handle = tokio::spawn(handle_ipc_client_with_timeout(
            server_stream,
            Arc::clone(&shared_snap),
            tx.subscribe(),
            refresh_tx.clone(),
            orch_wake_tx.clone(),
            cfg.clone(),
            paths.clone(),
            store.clone(),
            herdr.clone(),
            coord.clone(),
            Duration::from_secs(2),
        ));
        let (rh, mut wh) = client_stream.into_split();
        let req_line = serde_json::to_string(&IpcRequest {
            op: IpcOp::Status,
            terminal: None,
            include_all: false,
        })
        .unwrap()
            + "\n";
        wh.write_all(req_line.as_bytes()).await.unwrap();
        got_req.notified().await;
        allow_reply.notify_one();

        let mut reader = BufReader::new(rh);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let status_resp: CockpitSnapshot = serde_json::from_str(resp_line.trim()).unwrap();
        assert_eq!(status_resp.daemon_pid, Some(201));
        assert!(status_resp.herdr_connected);
        srv_handle.await.unwrap().unwrap();

        // 2. IpcOp::Refresh must wait for forced refresh and receive pid=202.
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let srv_handle = tokio::spawn(handle_ipc_client_with_timeout(
            server_stream,
            Arc::clone(&shared_snap),
            tx.subscribe(),
            refresh_tx.clone(),
            orch_wake_tx.clone(),
            cfg.clone(),
            paths.clone(),
            store.clone(),
            herdr.clone(),
            coord.clone(),
            Duration::from_secs(2),
        ));
        let (rh, mut wh) = client_stream.into_split();
        let req_line = serde_json::to_string(&IpcRequest {
            op: IpcOp::Refresh,
            terminal: None,
            include_all: false,
        })
        .unwrap()
            + "\n";
        wh.write_all(req_line.as_bytes()).await.unwrap();
        got_req.notified().await;
        allow_reply.notify_one();

        let mut reader = BufReader::new(rh);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let refresh_resp: CockpitSnapshot = serde_json::from_str(resp_line.trim()).unwrap();
        assert_eq!(refresh_resp.daemon_pid, Some(202));
        srv_handle.await.unwrap().unwrap();

        drop(refresh_tx);
        refresh_worker.await.unwrap();

        // 3. Bounded timeout fallback returns current shared_snap if refresh worker stalls.
        let (stalled_tx, _stalled_rx) = mpsc::channel::<RefreshRequest>(4);
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let srv_handle = tokio::spawn(handle_ipc_client_with_timeout(
            server_stream,
            Arc::clone(&shared_snap),
            tx.subscribe(),
            stalled_tx,
            orch_wake_tx,
            cfg,
            paths,
            store,
            herdr,
            coord,
            Duration::from_millis(15),
        ));
        let (rh, mut wh) = client_stream.into_split();
        let req_line = serde_json::to_string(&IpcRequest {
            op: IpcOp::Refresh,
            terminal: None,
            include_all: false,
        })
        .unwrap()
            + "\n";
        wh.write_all(req_line.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(rh);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let fallback_resp: CockpitSnapshot = serde_json::from_str(resp_line.trim()).unwrap();
        assert_eq!(fallback_resp.daemon_pid, Some(202));
        srv_handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_refresh_and_publish_snapshot_updates_shared_and_broadcast_consistently() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(tmp.path());
        let cfg = CockpitConfig::default();
        let store = CacheStore::open(&paths.db_file()).unwrap();
        let herdr = HerdrClient::new("/nonexistent-hc");
        let coord = ProviderCoordinator::new();
        let shared_snap = Arc::new(RwLock::new(CockpitSnapshot::default()));
        let (tx, mut rx) = broadcast::channel::<CockpitSnapshot>(8);
        let mut tracker = FleetTracker::new();
        let mut rpc: Option<HcRpcSession> = None;

        let pub_ctx = DaemonPublishCtx {
            cfg: &cfg,
            paths: &paths,
            store: &store,
            coord: &coord,
            shared_snap: &shared_snap,
            tx: &tx,
            pid: 777,
        };

        let out = refresh_and_publish_snapshot(
            &pub_ctx,
            &mut tracker,
            &herdr,
            &mut rpc,
            false,
            false,
            12_000,
        )
        .await;
        assert_eq!(out.daemon_pid, Some(777));
        assert!(!out.herdr_connected);
        assert_eq!(shared_snap.read().await.daemon_pid, Some(777));
        assert!(!shared_snap.read().await.herdr_connected);
        let bcast = rx.recv().await.unwrap();
        assert_eq!(bcast.daemon_pid, Some(777));
        assert!(!bcast.herdr_connected);
    }

    #[tokio::test]
    async fn test_drain_ready_event_lines_coalesces_and_preserves_partial() {
        let (mut writer, reader) = tokio::io::duplex(512);
        let mut buf_reader = BufReader::new(reader);
        let mut event_buf: Vec<u8> = Vec::new();
        let mut tracker = FleetTracker::new();
        let mut herdr_connected = false;

        writer
            .write_all(
                br#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"working"},"seq":1,"terminal":"t1","type":"status"}}
{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"blocked","kind":"permission","msg":"Run?"},"seq":2,"terminal":"t1","type":"status"}}
{"jsonrpc":"2.0","method":"event","params":{"data":{"turn":2},"seq":3,"terminal":"t1","type":"turn"#,
            )
            .await
            .unwrap();

        let (any_changed, stream_closed) = drain_ready_event_lines(
            &mut buf_reader,
            &mut event_buf,
            &mut tracker,
            &mut herdr_connected,
            5_000,
            MAX_DRAIN_EVENT_LINES,
        )
        .await;
        assert!(any_changed);
        assert!(!stream_closed);
        assert!(herdr_connected);
        assert_eq!(
            tracker
                .terminals
                .get("t1")
                .and_then(|t| t.status.as_ref())
                .map(|s| s.state.as_str()),
            Some("blocked")
        );
        assert!(
            !event_buf.is_empty(),
            "trailing partial line must stay buffered across drain"
        );

        // Complete the trailing line and close stream.
        writer.write_all(b"\"}}\n").await.unwrap();
        drop(writer);

        let (any_changed2, stream_closed2) = drain_ready_event_lines(
            &mut buf_reader,
            &mut event_buf,
            &mut tracker,
            &mut herdr_connected,
            5_100,
            MAX_DRAIN_EVENT_LINES,
        )
        .await;
        assert!(any_changed2);
        assert!(stream_closed2);
        assert_eq!(tracker.terminals.get("t1").map(|t| t.turn), Some(2));
        assert!(event_buf.is_empty());
    }

    #[tokio::test]
    async fn test_kill_and_reap_events_child() {
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().expect("spawned child should have pid");
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut slot = Some((child, stdout));

        kill_and_reap_events_child(&mut slot).await;
        assert!(slot.is_none());
        // Verify process is reaped (kill(pid, 0) fails with ESRCH).
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(rc, -1);
    }

    #[tokio::test]
    async fn test_watch_loops_before_orch_cycle_blocks_reused_caller_and_catches_dead_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let orch_store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
        let herdr = HerdrClient::new("/nonexistent-hc");
        let mut delivery_scheduler = DeliveryScheduler::new(herdr.clone());
        let mut tracker = FleetTracker::new();

        // t1 was reused (created_ms changed from 1_000 to 9_000).
        tracker.apply_snapshot(
            vec![crate::herdr::HcTerminal {
                address: "t1".to_string(),
                state: "running".to_string(),
                created_ms: Some(9_000),
                status: Some(crate::herdr::HcStatus {
                    state: "idle".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            10_000,
        );

        let dead_dir = tmp.path().join("dead-loop");
        std::fs::create_dir_all(&dead_dir).unwrap();
        let st = crate::loops::state::RunState {
            schema_version: crate::loops::state::LOOP_STATE_SCHEMA_VERSION,
            run_id: "loop-dead".to_string(),
            workflow: "worker-reviewer-critic".to_string(),
            status: crate::loops::state::RunStatus::Running,
            reason: None,
            sandbox: "bwrap".to_string(),
            sandbox_exposures: vec![],
            preflight: vec![],
            current_cycle: 1,
            max_cycles: 3,
            branch: None,
            base_commit: None,
            engine_pid: Some(u32::MAX),
            engine_start_ticks: Some(123),
            caller: Some(crate::loops::state::CallerInfo {
                terminal: "t1".to_string(),
                created_ms: 1_000,
            }),
            heartbeat_at_ms: 1_000,
            last_activity_at_ms: 1_000,
            started_at_ms: 1_000,
            finished_at_ms: None,
            executions: vec![],
            files: vec![],
            summary: String::new(),
        };
        crate::loops::state::save_state(&dead_dir, &st).unwrap();
        orch_store
            .register_loop(&crate::orch::store::LoopRunRecord {
                run_id: "loop-dead".to_string(),
                dir: dead_dir.display().to_string(),
                workflow: "worker-reviewer-critic".to_string(),
                status: "RUNNING".to_string(),
                caller_terminal: Some("t1".to_string()),
                caller_created_ms: Some(1_000),
                engine_pid: Some(u32::MAX),
                engine_start_ticks: Some(123),
                orch_run_id: None,
                notify_state: "pending".to_string(),
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();

        let updates = crate::loops::watch::watch_loops(
            &orch_store,
            &tracker,
            &crate::loops::watch::LinuxProcTable,
            10_000,
        )
        .unwrap();
        assert!(updates >= 1);

        let (_reconcile, schedule) = run_orch_cycle(
            &orch_store,
            &herdr,
            &mut delivery_scheduler,
            &tracker,
            10_000,
            "reconcile",
        )
        .await;
        assert_eq!(schedule.spawned_count, 0);
        let rec = orch_store.get_loop("loop-dead").unwrap().unwrap();
        assert_eq!(rec.status, "ERROR");
        assert_eq!(rec.notify_state, "undelivered");
    }
}
