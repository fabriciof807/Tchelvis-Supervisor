//! Process discovery, graceful shutdown, and detached restart for `tchelvis` daemons.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OurProc {
    pub pid: i32,
    pub argv: Vec<String>,
}

fn read_proc_uid(status_path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(status_path).ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

fn read_proc_argv(cmdline_path: &Path) -> Option<Vec<String>> {
    let bytes = fs::read(cmdline_path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mut argv: Vec<String> = bytes
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    while argv.last().is_some_and(String::is_empty) {
        argv.pop();
    }
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

fn is_our_daemon_argv(argv: &[String]) -> bool {
    let Some(arg0) = argv.first() else {
        return false;
    };
    let basename = Path::new(arg0).file_name().and_then(|s| s.to_str());
    if basename != Some("tchelvis") {
        return false;
    }
    match argv.get(1).map(String::as_str) {
        Some("daemon") => true,
        Some("supervisor") => argv.get(2).map(String::as_str) == Some("watch"),
        _ => false,
    }
}

/// Finds running `tchelvis daemon` and `tchelvis supervisor watch` processes owned by `uid`,
/// excluding `exclude_pid`.
pub fn find_our_processes(proc_root: &Path, uid: u32, exclude_pid: i32) -> Vec<OurProc> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut procs = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 0 || pid == exclude_pid {
            continue;
        }
        let dir = entry.path();
        if read_proc_uid(&dir.join("status")) != Some(uid) {
            continue;
        }
        let Some(argv) = read_proc_argv(&dir.join("cmdline")) else {
            continue;
        };
        if is_our_daemon_argv(&argv) {
            procs.push(OurProc { pid, argv });
        }
    }
    procs.sort_by_key(|p| p.pid);
    procs
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks process existence/permission.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Sends SIGTERM to `procs`, waits up to `grace`, and sends SIGKILL to any survivors.
/// Returns the PIDs that were signaled.
pub fn stop_processes(procs: &[OurProc], grace: Duration) -> Vec<i32> {
    let mut signaled = Vec::new();
    for proc_info in procs {
        if proc_info.pid <= 0 {
            continue;
        }
        // SAFETY: sending SIGTERM to a discovered user-owned process PID.
        let rc = unsafe { libc::kill(proc_info.pid, libc::SIGTERM) };
        if rc == 0 {
            signaled.push(proc_info.pid);
        }
    }
    if signaled.is_empty() {
        return signaled;
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if signaled.iter().all(|&pid| !pid_alive(pid)) {
            return signaled;
        }
        thread::sleep(Duration::from_millis(50));
    }

    for &pid in &signaled {
        if pid_alive(pid) {
            // SAFETY: sending SIGKILL to a process that ignored SIGTERM past grace.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    signaled
}

/// Drops `argv[0]` and returns the remaining subcommand arguments.
pub fn restart_args(argv: &[String]) -> Vec<String> {
    argv.iter().skip(1).cloned().collect()
}

fn systemd_unit_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "tchelvis.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Restarts previously stopped processes using `exe` (detached via `setsid`) or `systemctl --user`.
/// Returns human-readable descriptions of the restarted processes.
/// Daemon and supervisor processes write structured tracing logs directly to `tchelvis.log`
/// in `log_dir`, so stdout/stderr are connected to `/dev/null` instead of creating an empty
/// `daemon.log`.
pub fn restart(procs: &[OurProc], exe: &Path, log_dir: &Path) -> Result<Vec<String>> {
    if procs.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(log_dir)
        .with_context(|| format!("failed creating log directory {}", log_dir.display()))?;

    let use_systemd = systemd_unit_enabled();
    let mut systemd_restarted = false;
    let mut restarted = Vec::new();
    let log_path = log_dir.join("tchelvis.log");

    for proc_info in procs {
        let args = restart_args(&proc_info.argv);
        if args.is_empty() {
            continue;
        }
        if use_systemd && args.first().map(String::as_str) == Some("daemon") {
            if !systemd_restarted {
                let status = Command::new("systemctl")
                    .args(["--user", "restart", "tchelvis.service"])
                    .status()
                    .context("failed running systemctl --user restart tchelvis.service")?;
                if !status.success() {
                    anyhow::bail!("systemctl --user restart tchelvis.service exited with {status}");
                }
                restarted.push("tchelvis.service (systemd)".to_string());
                systemd_restarted = true;
            }
            continue;
        }

        let mut cmd = Command::new(exe);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid() is async-signal-safe and detaches the restarted daemon into its own session.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("failed restarting {} {}", exe.display(), args.join(" ")))?;
        restarted.push(format!(
            "tchelvis {} (pid {}, log {})",
            args.join(" "),
            child.id(),
            log_path.display()
        ));
    }

    Ok(restarted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fake_proc(root: &Path, pid: i32, uid: u32, argv: &[&str]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let mut cmdline = Vec::new();
        for arg in argv {
            cmdline.extend_from_slice(arg.as_bytes());
            cmdline.push(0);
        }
        fs::write(dir.join("cmdline"), cmdline).unwrap();
        fs::write(
            dir.join("status"),
            format!("Name:\ttchelvis\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
    }

    #[test]
    fn finds_daemon_and_watch_ignores_others() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        write_fake_proc(root, 101, 1000, &["tchelvis", "daemon"]);
        write_fake_proc(
            root,
            102,
            1000,
            &["/home/u/.local/bin/tchelvis", "supervisor", "watch"],
        );
        write_fake_proc(root, 103, 1000, &["tchelvis", "status"]);
        write_fake_proc(root, 104, 1000, &["tchelvis", "supervisor", "tick"]);
        write_fake_proc(root, 105, 0, &["tchelvis", "daemon"]);
        write_fake_proc(root, 106, 1000, &["tchelvis", "daemon"]);
        write_fake_proc(root, 107, 1000, &["cockpit", "daemon"]);

        let found = find_our_processes(root, 1000, 106);
        assert_eq!(
            found,
            vec![
                OurProc {
                    pid: 101,
                    argv: vec!["tchelvis".into(), "daemon".into()],
                },
                OurProc {
                    pid: 102,
                    argv: vec![
                        "/home/u/.local/bin/tchelvis".into(),
                        "supervisor".into(),
                        "watch".into(),
                    ],
                },
            ]
        );
    }

    #[test]
    fn restart_args_drops_argv0() {
        let argv: Vec<String> = vec!["tchelvis".into(), "supervisor".into(), "watch".into()];
        assert_eq!(
            restart_args(&argv),
            vec!["supervisor".to_string(), "watch".to_string()]
        );
    }

    #[test]
    fn restart_does_not_create_empty_daemon_log_and_points_to_tchelvis_log() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("state");
        let fake_exe = tmp.path().join("fake-tchelvis");
        crate::test_support::write_executable(&fake_exe, "#!/usr/bin/env sh\nexit 0\n");

        let procs = vec![OurProc {
            pid: 999999,
            argv: vec!["tchelvis".into(), "supervisor".into(), "watch".into()],
        }];
        let out = restart(&procs, &fake_exe, &log_dir).unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("tchelvis.log"),
            "expected tchelvis.log in output, got: {}",
            out[0]
        );
        assert!(!log_dir.join("daemon.log").exists());
        assert!(!log_dir.join("supervisor-watch.log").exists());
    }
}
