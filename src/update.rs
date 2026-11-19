//! Self-updater for `tchelvis` via GitHub Releases.

use crate::config::CockpitPaths;
use crate::procs;
use crate::version::{build_channel, build_commit, format_version};
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const REPO: &str = "fabriciof807/Tchelvis-Supervisor";
const USER_AGENT: &str = "tchelvis-updater";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Stable,
    Nightly,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Self::Stable),
            "nightly" => Some(Self::Nightly),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
        }
    }
}

/// Resolves the update channel: `--channel` flag > remembered file > `Stable`.
pub fn resolve_channel(flag: Option<Channel>, remembered: Option<Channel>) -> Channel {
    flag.or(remembered).unwrap_or(Channel::Stable)
}

/// Returns `~/.local/state/tchelvis/update_channel` (respecting `XDG_STATE_HOME`).
pub fn channel_file() -> PathBuf {
    if let Ok(paths) = CockpitPaths::resolve() {
        return paths.state_dir.join("update_channel");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let state_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    state_base.join("tchelvis").join("update_channel")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub commit: Option<String>,
    pub asset_url: String,
    pub sha_url: String,
}

/// Formats an HTTP status error with the status code and URL.
pub fn format_http_error(status: u16, url: &str) -> String {
    format!("HTTP {status} from {url}")
}

fn extract_commit_from_body(body: &str) -> Option<String> {
    let re = Regex::new(r"commit ([0-9a-f]{7,40})").ok()?;
    re.captures(body)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Parses a GitHub Release JSON payload for `channel` and `arch`.
pub fn parse_release(json: &Value, channel: Channel, arch: &str) -> Result<ReleaseInfo> {
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .context("release JSON missing tag_name")?
        .to_string();

    let version = match channel {
        Channel::Stable => tag.strip_prefix('v').unwrap_or(&tag).to_string(),
        Channel::Nightly => "nightly".to_string(),
    };

    let commit = if channel == Channel::Nightly {
        json.get("body")
            .and_then(Value::as_str)
            .and_then(extract_commit_from_body)
    } else {
        None
    };

    let expected_asset = format!("tchelvis-{version}-{arch}-unknown-linux-musl.tar.gz");
    let expected_sha = format!("{expected_asset}.sha256");

    let assets = json
        .get("assets")
        .and_then(Value::as_array)
        .context("release JSON missing assets array")?;

    let mut asset_url = None;
    let mut sha_url = None;
    for asset in assets {
        let Some(name) = asset.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(url) = asset.get("browser_download_url").and_then(Value::as_str) else {
            continue;
        };
        if name == expected_asset {
            asset_url = Some(url.to_string());
        } else if name == expected_sha {
            sha_url = Some(url.to_string());
        }
    }

    let asset_url = asset_url.with_context(|| {
        format!("release {tag} has no asset '{expected_asset}' for architecture {arch}")
    })?;
    let sha_url = sha_url.with_context(|| {
        format!("release {tag} has no checksum '{expected_sha}' for architecture {arch}")
    })?;

    Ok(ReleaseInfo {
        tag,
        version,
        commit,
        asset_url,
        sha_url,
    })
}

/// Determines whether an update should be applied given the current build metadata.
pub fn needs_update(
    channel: Channel,
    rel: &ReleaseInfo,
    cur_version: &str,
    cur_channel: Option<&str>,
    cur_commit: Option<&str>,
    force: bool,
) -> bool {
    if force {
        return true;
    }
    match channel {
        Channel::Stable => rel.version != cur_version || cur_channel != Some("stable"),
        Channel::Nightly => {
            if cur_channel == Some("stable") {
                return true;
            }
            match (rel.commit.as_deref(), cur_commit) {
                (Some(rel_sha), Some(cur_sha)) => rel_sha != cur_sha,
                _ => true,
            }
        }
    }
}

/// Verifies `bytes` against the first whitespace-delimited hex token in `sha_file_contents`.
pub fn verify_sha256(bytes: &[u8], sha_file_contents: &str) -> Result<()> {
    let expected = sha_file_contents
        .split_whitespace()
        .next()
        .context("sha256 file is empty")?
        .to_ascii_lowercase();

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());

    if actual != expected {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Extracts the `tchelvis` binary bytes from a `.tar.gz` archive.
pub fn extract_binary(tar_gz: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .context("failed reading tar.gz archive entries")?;

    for entry in entries {
        let mut entry = entry.context("corrupt tar entry")?;
        let path = entry.path().context("invalid path in tar entry")?;
        if path.file_name() == Some(OsStr::new("tchelvis")) && entry.header().entry_type().is_file()
        {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .context("failed reading tchelvis binary from archive")?;
            return Ok(buf);
        }
    }

    bail!("archive did not contain 'tchelvis' binary")
}

/// Verifies `archive` against `sha_file`, extracts `tchelvis`, and atomically replaces `target`.
pub fn install_bytes(target: &Path, archive: &[u8], sha_file: &str) -> Result<()> {
    verify_sha256(archive, sha_file)?;
    let binary = extract_binary(archive)?;

    let parent = target
        .parent()
        .context("target binary path has no parent directory")?;
    let tmp_path = parent.join(format!(".tchelvis.update.{}.tmp", std::process::id()));

    let write_res = fs::write(&tmp_path, &binary);
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        if err.kind() == ErrorKind::PermissionDenied {
            bail!(
                "permission denied writing to {} — check directory permissions or run install.sh",
                parent.display()
            );
        }
        return Err(err)
            .with_context(|| format!("failed writing temporary binary {}", tmp_path.display()));
    }

    if let Err(err) = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err)
            .with_context(|| format!("failed setting 0755 permissions on {}", tmp_path.display()));
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        if err.kind() == ErrorKind::PermissionDenied {
            bail!(
                "permission denied replacing {} — check directory permissions or run install.sh",
                target.display()
            );
        }
        return Err(err).with_context(|| format!("failed replacing {}", target.display()));
    }

    Ok(())
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
}

fn fetch_json(agent: &ureq::Agent, url: &str, channel: Channel) -> Result<Value> {
    match agent.get(url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => resp
            .into_json::<Value>()
            .with_context(|| format!("failed parsing JSON from {url}")),
        Err(ureq::Error::Status(404, _)) if channel == Channel::Stable => {
            bail!("No stable release yet — use `--channel nightly`");
        }
        Err(ureq::Error::Status(code, _)) => bail!("{}", format_http_error(code, url)),
        Err(ureq::Error::Transport(err)) => bail!("HTTP request failed for {url}: {err}"),
    }
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    match agent.get(url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => resp
            .into_string()
            .with_context(|| format!("failed reading text from {url}")),
        Err(ureq::Error::Status(code, _)) => bail!("{}", format_http_error(code, url)),
        Err(ureq::Error::Transport(err)) => bail!("HTTP request failed for {url}: {err}"),
    }
}

fn fetch_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    match agent.get(url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader()
                .read_to_end(&mut buf)
                .with_context(|| format!("failed reading response body from {url}"))?;
            Ok(buf)
        }
        Err(ureq::Error::Status(code, _)) => bail!("{}", format_http_error(code, url)),
        Err(ureq::Error::Transport(err)) => bail!("HTTP request failed for {url}: {err}"),
    }
}

fn read_remembered_channel(path: &Path) -> Option<Channel> {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| Channel::parse(&raw))
}

fn write_remembered_channel(path: &Path, channel: Channel) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating state dir {}", parent.display()))?;
    }
    fs::write(path, format!("{}\n", channel.as_str()))
        .with_context(|| format!("failed writing channel file {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UpdateCheckReport {
    pub current_version: String,
    pub current_commit: Option<String>,
    pub channel: Option<String>,
    pub channel_source: String,
    pub latest_version: String,
    pub latest_commit: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct BuildInfo<'a> {
    pub version: &'a str,
    pub channel: Option<&'a str>,
    pub commit: Option<&'a str>,
}

pub fn release_url_for(channel: Channel) -> String {
    match channel {
        Channel::Stable => format!("https://api.github.com/repos/{REPO}/releases/latest"),
        Channel::Nightly => format!("https://api.github.com/repos/{REPO}/releases/tags/nightly"),
    }
}

pub fn host_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => bail!("unsupported architecture '{other}' (expected x86_64 or aarch64)"),
    }
}

pub fn format_error_json(err: &anyhow::Error) -> String {
    serde_json::json!({ "error": format!("{err:#}") }).to_string()
}

pub fn check_update_with<F>(
    channel_flag: Option<Channel>,
    channel_path: &Path,
    arch: &str,
    build: BuildInfo<'_>,
    force: bool,
    fetch_release_json: F,
) -> Result<UpdateCheckReport>
where
    F: FnOnce(Channel, &str) -> Result<Value>,
{
    let remembered = read_remembered_channel(channel_path);
    let check_channel = resolve_channel(channel_flag, remembered);
    let (channel, channel_source) = match (channel_flag, remembered) {
        (Some(ch), _) => (Some(ch.as_str().to_string()), "flag".to_string()),
        (None, Some(ch)) => (Some(ch.as_str().to_string()), "saved".to_string()),
        (None, None) => (None, "none".to_string()),
    };

    let url = release_url_for(check_channel);
    let json = fetch_release_json(check_channel, &url)?;
    let rel = parse_release(&json, check_channel, arch)?;
    let update_available = needs_update(
        check_channel,
        &rel,
        build.version,
        build.channel,
        build.commit,
        force,
    );

    Ok(UpdateCheckReport {
        current_version: build.version.to_string(),
        current_commit: build.commit.map(str::to_string),
        channel,
        channel_source,
        latest_version: rel.version,
        latest_commit: rel.commit,
        update_available,
    })
}

/// Runs `tchelvis update`.
pub fn run_update(
    channel_flag: Option<Channel>,
    check: bool,
    json: bool,
    no_setup: bool,
    force: bool,
) -> Result<()> {
    if json && !check {
        println!(
            "{}",
            serde_json::json!({ "error": "--json is only supported with --check" })
        );
        std::process::exit(1);
    }

    let cf = channel_file();
    if check && json {
        let build = BuildInfo {
            version: env!("CARGO_PKG_VERSION"),
            channel: build_channel(),
            commit: build_commit(),
        };
        let agent = http_agent();
        let res = host_arch().and_then(|arch| {
            check_update_with(channel_flag, &cf, arch, build, force, |ch, url| {
                fetch_json(&agent, url, ch)
            })
        });
        match res {
            Ok(report) => {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            Err(err) => {
                println!("{}", format_error_json(&err));
                std::process::exit(1);
            }
        }
    }

    let channel = resolve_channel(channel_flag, read_remembered_channel(&cf));
    let arch = host_arch()?;
    let release_url = release_url_for(channel);

    let agent = http_agent();
    let json_payload = fetch_json(&agent, &release_url, channel)?;
    let rel = parse_release(&json_payload, channel, arch)?;

    let cur_version = env!("CARGO_PKG_VERSION");
    let cur_channel = build_channel();
    let cur_commit = build_commit();
    let update_needed = needs_update(channel, &rel, cur_version, cur_channel, cur_commit, force);

    let cur_label = format_version(cur_version, cur_channel, cur_commit);
    let avail_label = match (channel, rel.commit.as_deref()) {
        (Channel::Nightly, Some(sha)) => format!("nightly ({sha})"),
        _ => format!("{} ({})", rel.version, channel.as_str()),
    };

    if check {
        println!("{cur_label} → {avail_label}");
        std::process::exit(if update_needed { 0 } else { 1 });
    }

    if !update_needed {
        println!("Already up to date: {cur_label}");
        return Ok(());
    }

    let sha_contents = fetch_text(&agent, &rel.sha_url)?;
    let archive_bytes = fetch_bytes(&agent, &rel.asset_url)?;

    let exe_path =
        std::env::current_exe().context("failed to determine current executable path")?;
    install_bytes(&exe_path, &archive_bytes, &sha_contents)?;
    write_remembered_channel(&cf, channel)?;

    let mut restarted = Vec::new();
    if !no_setup {
        // SAFETY: geteuid() is always safe.
        let uid = unsafe { libc::geteuid() };
        let running = procs::find_our_processes(Path::new("/proc"), uid, std::process::id() as i32);
        let _stopped = procs::stop_processes(&running, Duration::from_secs(3));

        let setup_result = Command::new(&exe_path)
            .args(["setup", "--set-default-layout"])
            .status();

        // Restart whatever we stopped even if setup failed, so an update never leaves the
        // daemons down.
        let log_dir = CockpitPaths::resolve()
            .map(|p| p.state_dir)
            .unwrap_or_else(|_| {
                exe_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf()
            });
        let restart_result = procs::restart(&running, &exe_path, &log_dir);

        let setup_status = setup_result.with_context(|| {
            format!(
                "failed running `{} setup --set-default-layout`; retry with: tchelvis setup --set-default-layout",
                exe_path.display()
            )
        })?;
        if !setup_status.success() {
            bail!(
                "`tchelvis setup --set-default-layout` exited with {setup_status}; binary is updated — retry with: tchelvis setup --set-default-layout"
            );
        }
        restarted = restart_result?;
    }

    println!(
        "Updated tchelvis: {cur_label} → {avail_label} (channel: {})",
        channel.as_str()
    );
    if !restarted.is_empty() {
        println!("Restarted daemons:");
        for item in &restarted {
            println!("  - {item}");
        }
        println!(
            "Note: restarted processes inherited this shell's environment (including any TCHELVIS_LLM_* / GPU variables)."
        );
    }
    println!("Restart Herdr Desktop to reload the mod.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn make_tar_gz(binary_bytes: &[u8]) -> (Vec<u8>, String) {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(binary_bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "tchelvis", binary_bytes)
                .unwrap();
            builder.finish().unwrap();
        }
        let archive = gz.finish().unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&archive);
        let hex = format!("{:x}", hasher.finalize());
        let sha_file = format!("{hex}  tchelvis-0.2.0-x86_64-unknown-linux-musl.tar.gz\n");
        (archive, sha_file)
    }

    #[test]
    fn parse_release_stable() {
        let json = serde_json::json!({
            "tag_name": "v0.3.0",
            "body": "Release 0.3.0",
            "assets": [
                {
                    "name": "tchelvis-0.3.0-x86_64-unknown-linux-musl.tar.gz",
                    "browser_download_url": "https://example.com/tchelvis.tar.gz"
                },
                {
                    "name": "tchelvis-0.3.0-x86_64-unknown-linux-musl.tar.gz.sha256",
                    "browser_download_url": "https://example.com/tchelvis.tar.gz.sha256"
                }
            ]
        });
        let rel = parse_release(&json, Channel::Stable, "x86_64").unwrap();
        assert_eq!(rel.tag, "v0.3.0");
        assert_eq!(rel.version, "0.3.0");
        assert_eq!(rel.asset_url, "https://example.com/tchelvis.tar.gz");
        assert_eq!(rel.sha_url, "https://example.com/tchelvis.tar.gz.sha256");
    }

    #[test]
    fn parse_release_nightly_reads_commit() {
        let json = serde_json::json!({
            "tag_name": "nightly",
            "body": "Automated nightly\ncommit a1b2c3d4 · 2026-11-15",
            "assets": [
                {
                    "name": "tchelvis-nightly-aarch64-unknown-linux-musl.tar.gz",
                    "browser_download_url": "https://example.com/n.tar.gz"
                },
                {
                    "name": "tchelvis-nightly-aarch64-unknown-linux-musl.tar.gz.sha256",
                    "browser_download_url": "https://example.com/n.tar.gz.sha256"
                }
            ]
        });
        let rel = parse_release(&json, Channel::Nightly, "aarch64").unwrap();
        assert_eq!(rel.tag, "nightly");
        assert_eq!(rel.version, "nightly");
        assert_eq!(rel.commit.as_deref(), Some("a1b2c3d4"));
        assert_eq!(rel.asset_url, "https://example.com/n.tar.gz");
        assert_eq!(rel.sha_url, "https://example.com/n.tar.gz.sha256");
    }

    #[test]
    fn parse_release_missing_asset_errors() {
        let json = serde_json::json!({
            "tag_name": "v0.3.0",
            "assets": []
        });
        let err = parse_release(&json, Channel::Stable, "aarch64").unwrap_err();
        assert!(
            err.to_string().contains("aarch64"),
            "error should name arch: {err}"
        );
    }

    #[test]
    fn needs_update_matrix() {
        let stable_same = ReleaseInfo {
            tag: "v0.2.0".into(),
            version: "0.2.0".into(),
            commit: None,
            asset_url: String::new(),
            sha_url: String::new(),
        };
        let stable_newer = ReleaseInfo {
            tag: "v0.3.0".into(),
            version: "0.3.0".into(),
            commit: None,
            asset_url: String::new(),
            sha_url: String::new(),
        };
        assert!(!needs_update(
            Channel::Stable,
            &stable_same,
            "0.2.0",
            Some("stable"),
            None,
            false
        ));
        assert!(needs_update(
            Channel::Stable,
            &stable_newer,
            "0.2.0",
            Some("stable"),
            None,
            false
        ));

        let nightly_rel = ReleaseInfo {
            tag: "nightly".into(),
            version: "nightly".into(),
            commit: Some("deadbeef".into()),
            asset_url: String::new(),
            sha_url: String::new(),
        };
        assert!(!needs_update(
            Channel::Nightly,
            &nightly_rel,
            "0.2.0",
            Some("nightly"),
            Some("deadbeef"),
            false
        ));
        assert!(needs_update(
            Channel::Nightly,
            &nightly_rel,
            "0.2.0",
            Some("nightly"),
            Some("cafebabe"),
            false
        ));
        assert!(needs_update(
            Channel::Stable,
            &stable_same,
            "0.2.0",
            Some("stable"),
            None,
            true
        ));
    }

    #[test]
    fn resolve_channel_precedence() {
        assert_eq!(
            resolve_channel(Some(Channel::Nightly), Some(Channel::Stable)),
            Channel::Nightly
        );
        assert_eq!(
            resolve_channel(None, Some(Channel::Nightly)),
            Channel::Nightly
        );
        assert_eq!(resolve_channel(None, None), Channel::Stable);
    }

    #[test]
    fn install_bytes_replaces_atomically() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("tchelvis");
        fs::write(&target, b"old-binary").unwrap();

        let (archive, sha) = make_tar_gz(b"new-binary-payload");
        install_bytes(&target, &archive, &sha).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new-binary-payload");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn install_bytes_rejects_bad_sha_and_keeps_original() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("tchelvis");
        fs::write(&target, b"old-binary").unwrap();

        let (archive, _) = make_tar_gz(b"new-binary-payload");
        let bad_sha =
            "0000000000000000000000000000000000000000000000000000000000000000  tchelvis.tar.gz\n";
        let err = install_bytes(&target, &archive, bad_sha).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("sha256"));
        assert_eq!(fs::read(&target).unwrap(), b"old-binary");
    }

    #[test]
    fn install_bytes_unwritable_dir_errors_and_keeps_original() {
        // SAFETY: geteuid() is always safe.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("tchelvis");
        fs::write(&target, b"old-binary").unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o555)).unwrap();

        let (archive, sha) = make_tar_gz(b"new-binary-payload");
        let res = install_bytes(&target, &archive, &sha);

        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old-binary");
    }

    #[test]
    fn http_error_message_names_status() {
        let msg = format_http_error(403, "https://api.github.com/repos/x/y");
        assert_eq!(msg, "HTTP 403 from https://api.github.com/repos/x/y");
    }

    fn fake_stable_release(ver: &str) -> Value {
        serde_json::json!({
            "tag_name": format!("v{ver}"),
            "body": format!("Release {ver}"),
            "assets": [
                {
                    "name": format!("tchelvis-{ver}-x86_64-unknown-linux-musl.tar.gz"),
                    "browser_download_url": "https://example.com/stable.tar.gz"
                },
                {
                    "name": format!("tchelvis-{ver}-x86_64-unknown-linux-musl.tar.gz.sha256"),
                    "browser_download_url": "https://example.com/stable.tar.gz.sha256"
                }
            ]
        })
    }

    fn fake_nightly_release(sha: &str) -> Value {
        serde_json::json!({
            "tag_name": "nightly",
            "body": format!("Automated nightly\ncommit {sha} · 2026-11-19"),
            "assets": [
                {
                    "name": "tchelvis-nightly-x86_64-unknown-linux-musl.tar.gz",
                    "browser_download_url": "https://example.com/nightly.tar.gz"
                },
                {
                    "name": "tchelvis-nightly-x86_64-unknown-linux-musl.tar.gz.sha256",
                    "browser_download_url": "https://example.com/nightly.tar.gz.sha256"
                }
            ]
        })
    }

    #[test]
    fn check_json_saved_stable_channel_shape() {
        let tmp = TempDir::new().unwrap();
        let cf = tmp.path().join("update_channel");
        write_remembered_channel(&cf, Channel::Stable).unwrap();

        let report = check_update_with(
            None,
            &cf,
            "x86_64",
            BuildInfo {
                version: "0.2.0",
                channel: Some("stable"),
                commit: None,
            },
            false,
            |ch, _url| {
                assert_eq!(ch, Channel::Stable);
                Ok(fake_stable_release("0.2.0"))
            },
        )
        .unwrap();

        let val = serde_json::to_value(&report).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "current_version": "0.2.0",
                "current_commit": null,
                "channel": "stable",
                "channel_source": "saved",
                "latest_version": "0.2.0",
                "latest_commit": null,
                "update_available": false
            })
        );
    }

    #[test]
    fn check_json_saved_nightly_channel_shape() {
        let tmp = TempDir::new().unwrap();
        let cf = tmp.path().join("update_channel");
        write_remembered_channel(&cf, Channel::Nightly).unwrap();

        let report = check_update_with(
            None,
            &cf,
            "x86_64",
            BuildInfo {
                version: "0.2.0",
                channel: Some("nightly"),
                commit: Some("1111111"),
            },
            false,
            |ch, _url| {
                assert_eq!(ch, Channel::Nightly);
                Ok(fake_nightly_release("2222222"))
            },
        )
        .unwrap();

        let val = serde_json::to_value(&report).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "current_version": "0.2.0",
                "current_commit": "1111111",
                "channel": "nightly",
                "channel_source": "saved",
                "latest_version": "nightly",
                "latest_commit": "2222222",
                "update_available": true
            })
        );
    }

    #[test]
    fn check_json_no_saved_channel_reports_none_and_null() {
        let tmp = TempDir::new().unwrap();
        let cf = tmp.path().join("update_channel");

        let report = check_update_with(
            None,
            &cf,
            "x86_64",
            BuildInfo {
                version: "0.2.0",
                channel: Some("stable"),
                commit: None,
            },
            false,
            |ch, _url| {
                assert_eq!(ch, Channel::Stable);
                Ok(fake_stable_release("0.3.0"))
            },
        )
        .unwrap();

        let val = serde_json::to_value(&report).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "current_version": "0.2.0",
                "current_commit": null,
                "channel": null,
                "channel_source": "none",
                "latest_version": "0.3.0",
                "latest_commit": null,
                "update_available": true
            })
        );
    }

    #[test]
    fn check_json_flag_overrides_saved_channel() {
        let tmp = TempDir::new().unwrap();
        let cf = tmp.path().join("update_channel");
        write_remembered_channel(&cf, Channel::Stable).unwrap();

        let report = check_update_with(
            Some(Channel::Nightly),
            &cf,
            "x86_64",
            BuildInfo {
                version: "0.2.0",
                channel: Some("nightly"),
                commit: Some("a1b2c3d"),
            },
            false,
            |ch, _url| {
                assert_eq!(ch, Channel::Nightly);
                Ok(fake_nightly_release("a1b2c3d"))
            },
        )
        .unwrap();

        let val = serde_json::to_value(&report).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "current_version": "0.2.0",
                "current_commit": "a1b2c3d",
                "channel": "nightly",
                "channel_source": "flag",
                "latest_version": "nightly",
                "latest_commit": "a1b2c3d",
                "update_available": false
            })
        );
    }

    #[test]
    fn check_json_network_error_formats_json_error() {
        let tmp = TempDir::new().unwrap();
        let cf = tmp.path().join("update_channel");

        let err = check_update_with(
            None,
            &cf,
            "x86_64",
            BuildInfo {
                version: "0.2.0",
                channel: None,
                commit: None,
            },
            false,
            |_ch, url| bail!("HTTP request failed for {url}: connection refused"),
        )
        .unwrap_err();

        let json_str = format_error_json(&err);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap()
            .contains("connection refused"));
    }
}
