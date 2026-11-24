use crate::loops::sandbox::SandboxedCommand;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_PARSED_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_BYTES: usize = 64 * 1024 * 1024;
const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const POLL_SLICE_MS: i32 = 25;
/// Reading all of `/proc` is expensive: the tree is sampled, not followed.
const TREE_SCAN_INTERVAL: Duration = Duration::from_millis(500);
/// How long to read what is left in the pipes after the stage ended.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

static ACTIVE_STAGE_PIDS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();

fn active_stage_pids() -> &'static Mutex<HashSet<i32>> {
    ACTIVE_STAGE_PIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

struct ActiveStageGuard(i32);

impl ActiveStageGuard {
    fn register(pid: i32) -> Self {
        let mut set = active_stage_pids()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.insert(pid);
        Self(pid)
    }
}

impl Drop for ActiveStageGuard {
    fn drop(&mut self) {
        let mut set = active_stage_pids()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.remove(&self.0);
    }
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn monotonic_ms(&self) -> u64;
}

#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn monotonic_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub stall: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Stalled,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
struct ProcStat {
    pid: i32,
    state: char,
    ppid: i32,
    pgrp: i32,
    session: i32,
    start_ticks: u64,
}

fn read_proc_stat(pid: i32) -> Option<ProcStat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = raw.rfind(')')?;
    let rest = raw.get(rparen + 1..)?.trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    let state = fields[0].chars().next()?;
    let ppid = fields[1].parse::<i32>().ok()?;
    let pgrp = fields[2].parse::<i32>().ok()?;
    let session = fields[3].parse::<i32>().ok()?;
    let start_ticks = fields[19].parse::<u64>().ok()?;
    Some(ProcStat {
        pid,
        state,
        ppid,
        pgrp,
        session,
        start_ticks,
    })
}

fn scan_proc_stats() -> Vec<ProcStat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut stats = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        if let Some(st) = read_proc_stat(pid) {
            stats.push(st);
        }
    }
    stats
}

struct TreeContext<'a> {
    root_pid: i32,
    root_start_ticks: Option<u64>,
    our_pid: i32,
    our_sid: i32,
    cwd: &'a Path,
    /// Every process seen in the tree, with its start time: a pid alone may be reused.
    observed: &'a mut HashMap<i32, u64>,
}

fn collect_tree_pids(ctx: &mut TreeContext<'_>) -> Vec<i32> {
    let stats = scan_proc_stats();
    let mut by_ppid: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut stat_by_pid: HashMap<i32, ProcStat> = HashMap::new();
    for st in &stats {
        by_ppid.entry(st.ppid).or_default().push(st.pid);
        stat_by_pid.insert(st.pid, *st);
    }

    let active_roots: HashSet<i32> = {
        let guard = active_stage_pids()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };

    let mut queue: VecDeque<i32> = VecDeque::new();
    let mut visited: HashSet<i32> = HashSet::new();

    let is_root = |st: &ProcStat| ctx.root_start_ticks.is_none_or(|t| st.start_ticks == t);
    if stat_by_pid.get(&ctx.root_pid).is_some_and(is_root) {
        queue.push_back(ctx.root_pid);
        visited.insert(ctx.root_pid);
    }
    for (&pid, &start_ticks) in ctx.observed.iter() {
        if let Some(st) = stat_by_pid.get(&pid) {
            if st.state != 'Z' && st.start_ticks == start_ticks && visited.insert(pid) {
                queue.push_back(pid);
            }
        }
    }
    for st in &stats {
        if st.pgrp == ctx.root_pid
            && st.pid != ctx.our_pid
            && ctx.root_start_ticks.is_none_or(|t| st.start_ticks >= t)
            && visited.insert(st.pid)
        {
            queue.push_back(st.pid);
        }
        // Catch grandchildren that called setsid() and were reparented to us via PR_SET_CHILD_SUBREAPER.
        if st.ppid == ctx.our_pid
            && st.pid != ctx.root_pid
            && st.state != 'Z'
            && st.session != ctx.our_sid
            && !active_roots.contains(&st.pid)
            && ctx.root_start_ticks.is_none_or(|t| st.start_ticks >= t)
        {
            let matches_cwd = fs::read_link(format!("/proc/{}/cwd", st.pid))
                .ok()
                .is_some_and(|p| p == ctx.cwd);
            if (ctx.observed.contains_key(&st.pid) || matches_cwd || active_roots.len() <= 1)
                && visited.insert(st.pid)
            {
                queue.push_back(st.pid);
            }
        }
    }

    while let Some(pid) = queue.pop_front() {
        if let Some(children) = by_ppid.get(&pid) {
            for &child in children {
                if child != ctx.our_pid && visited.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    let mut live = Vec::new();
    for pid in visited {
        if pid != ctx.our_pid {
            if let Some(st) = stat_by_pid.get(&pid) {
                ctx.observed.insert(pid, st.start_ticks);
                if st.state != 'Z' {
                    live.push(pid);
                }
            }
        }
    }
    live
}

fn reap_reparented(observed: &HashMap<i32, u64>, root_pid: i32) {
    for &pid in observed.keys() {
        if pid == root_pid || pid <= 0 {
            continue;
        }
        let mut status: libc::c_int = 0;
        // SAFETY: WNOHANG waitpid on a specific observed descendant PID we own if reparented;
        // returns -1/ECHILD if not our direct child.
        unsafe {
            libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
    }
}

fn terminate_process_tree(
    child: &mut Child,
    child_exited: &mut Option<std::process::ExitStatus>,
    ctx: &mut TreeContext<'_>,
) {
    let live = collect_tree_pids(ctx);
    if child_exited.is_some() && live.is_empty() {
        reap_reparented(ctx.observed, ctx.root_pid);
        return;
    }

    // SAFETY: root_pid is our child's process group id; sending SIGTERM to the group.
    unsafe {
        libc::kill(-ctx.root_pid, libc::SIGTERM);
    }
    for &pid in &live {
        // SAFETY: sending SIGTERM to each discovered descendant pid.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }

    let deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        if child_exited.is_none() {
            if let Ok(Some(st)) = child.try_wait() {
                *child_exited = Some(st);
            }
        }
        reap_reparented(ctx.observed, ctx.root_pid);
        let remaining = collect_tree_pids(ctx);
        if child_exited.is_some() && remaining.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }

    // Grace expired: escalate to SIGKILL on group and all remaining descendants until none remain.
    for _ in 0..40 {
        // SAFETY: escalating to SIGKILL on the process group.
        unsafe {
            libc::kill(-ctx.root_pid, libc::SIGKILL);
        }
        let remaining = collect_tree_pids(ctx);
        for &pid in &remaining {
            // SAFETY: escalating to SIGKILL on each surviving descendant.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        if child_exited.is_none() {
            if let Ok(Some(st)) = child.try_wait() {
                *child_exited = Some(st);
            }
        }
        reap_reparented(ctx.observed, ctx.root_pid);
        let after = collect_tree_pids(ctx);
        if child_exited.is_some() && after.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(15));
    }

    if child_exited.is_none() {
        if let Ok(st) = child.wait() {
            *child_exited = Some(st);
        }
    }
    reap_reparented(ctx.observed, ctx.root_pid);
}

fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    // SAFETY: fcntl on a valid open file descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL) falhou");
    }
    // SAFETY: setting O_NONBLOCK on a valid open pipe fd.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL) falhou");
    }
    Ok(())
}

fn write_to_log(
    log_file: &mut File,
    bytes: &[u8],
    written: &mut usize,
    truncated: &mut bool,
) -> Result<()> {
    write_to_log_with_cap(log_file, bytes, written, truncated, MAX_LOG_BYTES)
}

fn write_to_log_with_cap(
    log_file: &mut File,
    bytes: &[u8],
    written: &mut usize,
    truncated: &mut bool,
    cap: usize,
) -> Result<()> {
    if *truncated || bytes.is_empty() {
        return Ok(());
    }
    let remaining = cap.saturating_sub(*written);
    if bytes.len() <= remaining {
        log_file.write_all(bytes)?;
        *written += bytes.len();
    } else {
        if remaining > 0 {
            log_file.write_all(&bytes[..remaining])?;
            *written += remaining;
        }
        *truncated = true;
        log_file.write_all(b"\n[tchelvis: log truncado em 64 MiB]\n")?;
    }
    Ok(())
}

fn feed_stdout_lines(
    mut rest: &[u8],
    line_buf: &mut Vec<u8>,
    line_truncated: &mut bool,
    on_line: &mut dyn FnMut(&str),
) {
    while !rest.is_empty() {
        if let Some(nl_pos) = rest.iter().position(|&b| b == b'\n') {
            let segment = &rest[..nl_pos];
            if !*line_truncated {
                let avail = MAX_PARSED_LINE_BYTES.saturating_sub(line_buf.len());
                let take = segment.len().min(avail);
                line_buf.extend_from_slice(&segment[..take]);
            }
            if line_buf.last() == Some(&b'\r') {
                line_buf.pop();
            }
            let line = String::from_utf8_lossy(line_buf);
            on_line(&line);
            line_buf.clear();
            *line_truncated = false;
            rest = &rest[nl_pos + 1..];
        } else {
            if !*line_truncated {
                let avail = MAX_PARSED_LINE_BYTES.saturating_sub(line_buf.len());
                let take = rest.len().min(avail);
                line_buf.extend_from_slice(&rest[..take]);
                if rest.len() > avail {
                    *line_truncated = true;
                }
            }
            break;
        }
    }
}

fn flush_trailing_line(line_buf: &mut Vec<u8>, on_line: &mut dyn FnMut(&str)) {
    if !line_buf.is_empty() {
        if line_buf.last() == Some(&b'\r') {
            line_buf.pop();
        }
        let line = String::from_utf8_lossy(line_buf);
        on_line(&line);
        line_buf.clear();
    }
}

enum ReadStreamResult {
    Data(usize),
    WouldBlock,
    Eof,
}

fn read_nonblocking_fd(fd: libc::c_int, buf: &mut [u8]) -> Result<ReadStreamResult> {
    // SAFETY: fd is open and buf is a valid mutable byte slice.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        Ok(ReadStreamResult::Data(n as usize))
    } else if n == 0 {
        Ok(ReadStreamResult::Eof)
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock
            || err.kind() == std::io::ErrorKind::Interrupted
        {
            Ok(ReadStreamResult::WouldBlock)
        } else {
            Err(err).context("falha ao ler pipe do processo")
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_process(
    cmd: SandboxedCommand,
    stdin_data: &[u8],
    log: &Path,
    limits: Limits,
    clock: &dyn Clock,
    cancel: &AtomicBool,
    on_line: &mut dyn FnMut(&str),
    probe_activity: &mut dyn FnMut() -> bool,
) -> Result<Outcome> {
    let Some((prog, args)) = cmd.argv.split_first() else {
        bail!("comando vazio em run_process");
    };

    if let Some(parent) = log.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("falha ao criar diretório de logs {}", parent.display())
            })?;
        }
    }
    let mut log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log)
        .with_context(|| format!("falha ao abrir arquivo de log {}", log.display()))?;

    // Become a subreaper so grandchildren that call setsid() are reparented to us.
    // SAFETY: prctl(PR_SET_CHILD_SUBREAPER, 1, ...) is safe for the current process.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }

    let mut command = Command::new(prog);
    command
        .args(args)
        .current_dir(&cmd.cwd)
        .env_clear()
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // SAFETY: setpgid and prctl are async-signal-safe. The stage gets its own process group and
    // dies with the engine even when the engine is killed.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("falha ao iniciar processo {}", prog.to_string_lossy()))?;

    let root_pid = child.id() as i32;
    let _active_guard = ActiveStageGuard::register(root_pid);
    let root_start_ticks = read_proc_stat(root_pid).map(|s| s.start_ticks);
    let our_pid = std::process::id() as i32;
    // SAFETY: getsid(0) queries our own session id.
    let our_sid = unsafe { libc::getsid(0) };

    let mut stdin_pipe = if stdin_data.is_empty() {
        drop(child.stdin.take());
        None
    } else {
        let f = child.stdin.take().context("stdin pipe ausente")?;
        set_nonblocking(f.as_raw_fd())?;
        Some(f)
    };
    let mut stdout_pipe = child.stdout.take().inspect(|f| {
        let _ = set_nonblocking(f.as_raw_fd());
    });
    let mut stderr_pipe = child.stderr.take().inspect(|f| {
        let _ = set_nonblocking(f.as_raw_fd());
    });

    let mut stdin_offset: usize = 0;
    let mut log_bytes_written: usize = 0;
    let mut log_truncated = false;
    let mut stdout_line_buf: Vec<u8> = Vec::new();
    let mut stdout_line_truncated = false;
    let mut read_buf = vec![0u8; 64 * 1024];

    let mut observed_descendants: HashMap<i32, u64> = HashMap::new();
    let mut tree_ctx = TreeContext {
        root_pid,
        root_start_ticks,
        our_pid,
        our_sid,
        cwd: &cmd.cwd,
        observed: &mut observed_descendants,
    };
    let mut child_exited: Option<std::process::ExitStatus> = None;

    // Whatever goes wrong while watching (a full disk, for example), the stage must not survive.
    let mut watch = || -> Result<Option<Outcome>> {
        let mut next_scan = Instant::now();
        let mut drain_deadline: Option<Instant> = None;
        let start_ms = clock.monotonic_ms();
        let mut last_activity_ms = start_ms;
        let timeout_ms = limits.timeout.as_millis() as u64;
        let stall_ms = limits.stall.as_millis() as u64;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Some(Outcome::Cancelled));
            }

            let mut poll_fds: Vec<libc::pollfd> = Vec::with_capacity(3);
            let mut idx_stdout = None;
            let mut idx_stderr = None;
            let mut idx_stdin = None;

            if let Some(ref f) = stdout_pipe {
                idx_stdout = Some(poll_fds.len());
                poll_fds.push(libc::pollfd {
                    fd: f.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if let Some(ref f) = stderr_pipe {
                idx_stderr = Some(poll_fds.len());
                poll_fds.push(libc::pollfd {
                    fd: f.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if let Some(ref f) = stdin_pipe {
                idx_stdin = Some(poll_fds.len());
                poll_fds.push(libc::pollfd {
                    fd: f.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }

            // With no pipe left this is just a pause.
            // SAFETY: poll_fds contains valid open fds for this iteration.
            let rc = unsafe {
                libc::poll(
                    poll_fds.as_mut_ptr(),
                    poll_fds.len() as libc::nfds_t,
                    POLL_SLICE_MS,
                )
            };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err).context("poll falhou no monitor do processo");
                }
            }

            let mut saw_io_bytes = false;

            if let Some(idx) = idx_stdin {
                let revents = poll_fds[idx].revents;
                if (revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                    stdin_pipe = None;
                } else if (revents & libc::POLLOUT) != 0 {
                    if let Some(ref f) = stdin_pipe {
                        while stdin_offset < stdin_data.len() {
                            let slice = &stdin_data[stdin_offset..];
                            // SAFETY: f is an open non-blocking pipe fd.
                            let written = unsafe {
                                libc::write(f.as_raw_fd(), slice.as_ptr().cast(), slice.len())
                            };
                            if written > 0 {
                                stdin_offset += written as usize;
                            } else if written == 0 {
                                break;
                            } else {
                                let err = std::io::Error::last_os_error();
                                match err.kind() {
                                    std::io::ErrorKind::WouldBlock => break,
                                    std::io::ErrorKind::Interrupted => continue,
                                    std::io::ErrorKind::BrokenPipe => {
                                        stdin_pipe = None;
                                        break;
                                    }
                                    _ => {
                                        stdin_pipe = None;
                                        break;
                                    }
                                }
                            }
                        }
                        if stdin_offset >= stdin_data.len() {
                            stdin_pipe = None;
                        }
                    }
                }
            }

            if let Some(idx) = idx_stdout {
                let revents = poll_fds[idx].revents;
                if (revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                    let fd = stdout_pipe.as_ref().map(|f| f.as_raw_fd());
                    if let Some(fd) = fd {
                        loop {
                            match read_nonblocking_fd(fd, &mut read_buf)? {
                                ReadStreamResult::Data(n) => {
                                    saw_io_bytes = true;
                                    let chunk = &read_buf[..n];
                                    write_to_log(
                                        &mut log_file,
                                        chunk,
                                        &mut log_bytes_written,
                                        &mut log_truncated,
                                    )?;
                                    feed_stdout_lines(
                                        chunk,
                                        &mut stdout_line_buf,
                                        &mut stdout_line_truncated,
                                        on_line,
                                    );
                                }
                                ReadStreamResult::WouldBlock => break,
                                ReadStreamResult::Eof => {
                                    flush_trailing_line(&mut stdout_line_buf, on_line);
                                    stdout_pipe = None;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if let Some(idx) = idx_stderr {
                let revents = poll_fds[idx].revents;
                if (revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                    let fd = stderr_pipe.as_ref().map(|f| f.as_raw_fd());
                    if let Some(fd) = fd {
                        loop {
                            match read_nonblocking_fd(fd, &mut read_buf)? {
                                ReadStreamResult::Data(n) => {
                                    saw_io_bytes = true;
                                    write_to_log(
                                        &mut log_file,
                                        &read_buf[..n],
                                        &mut log_bytes_written,
                                        &mut log_truncated,
                                    )?;
                                }
                                ReadStreamResult::WouldBlock => break,
                                ReadStreamResult::Eof => {
                                    stderr_pipe = None;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Sample descendants while their parents are alive.
            if Instant::now() >= next_scan {
                let _ = collect_tree_pids(&mut tree_ctx);
                next_scan = Instant::now() + TREE_SCAN_INTERVAL;
            }

            if child_exited.is_none() {
                if let Ok(Some(st)) = child.try_wait() {
                    child_exited = Some(st);
                    // The stage ended: a background process must neither survive nor, by holding the
                    // pipes open, delay the result.
                    terminate_process_tree(&mut child, &mut child_exited, &mut tree_ctx);
                    drain_deadline = Some(Instant::now() + DRAIN_GRACE);
                }
            }

            if let Some(deadline) = drain_deadline {
                let drained = stdout_pipe.is_none() && stderr_pipe.is_none();
                if drained || Instant::now() >= deadline {
                    flush_trailing_line(&mut stdout_line_buf, on_line);
                    return Ok(None);
                }
                continue;
            }

            let probed_activity = probe_activity();
            let now = clock.monotonic_ms();
            if saw_io_bytes || probed_activity {
                last_activity_ms = now;
            }

            if cancel.load(Ordering::Relaxed) {
                return Ok(Some(Outcome::Cancelled));
            }

            if now.saturating_sub(start_ms) >= timeout_ms {
                return Ok(Some(Outcome::TimedOut));
            }

            if now.saturating_sub(last_activity_ms) >= stall_ms {
                return Ok(Some(Outcome::Stalled));
            }
        }
    };
    let watched = watch();
    terminate_process_tree(&mut child, &mut child_exited, &mut tree_ctx);
    let _ = log_file.flush();
    if let Some(outcome) = watched? {
        return Ok(outcome);
    }

    let status = child_exited.context("processo terminou sem código de saída")?;
    if let Some(code) = status.code() {
        Ok(Outcome::Exited(code))
    } else if let Some(sig) = status.signal() {
        Ok(Outcome::Signaled(sig))
    } else {
        Ok(Outcome::Exited(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct PidGuard(Option<i32>);

    impl Drop for PidGuard {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    struct StepClock {
        mono_ms: AtomicU64,
        step_ms: u64,
    }

    impl StepClock {
        fn new(step_ms: u64) -> Self {
            Self {
                mono_ms: AtomicU64::new(1_000),
                step_ms,
            }
        }
    }

    impl Clock for StepClock {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000 + self.mono_ms.load(Ordering::Relaxed) as i64
        }

        fn monotonic_ms(&self) -> u64 {
            self.mono_ms.fetch_add(self.step_ms, Ordering::Relaxed)
        }
    }

    fn pid_is_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let stat_path = format!("/proc/{pid}/stat");
        if let Ok(stat) = fs::read_to_string(&stat_path) {
            if let Some(rparen) = stat.rfind(')') {
                let state = stat[rparen + 1..].trim_start().chars().next();
                if state == Some('Z') {
                    return false;
                }
                return true;
            }
        }
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_pid_gone(pid: i32) -> bool {
        for _ in 0..40 {
            if !pid_is_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        !pid_is_alive(pid)
    }

    fn read_pid_file(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(raw) = fs::read_to_string(path) {
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    return pid;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for pid file {}", path.display());
    }

    fn script_cmd(script: &Path, args: &[&str], cwd: &Path) -> SandboxedCommand {
        let mut argv = vec![OsString::from(script)];
        argv.extend(args.iter().map(OsString::from));
        SandboxedCommand {
            argv,
            env: vec![
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
                ),
                (
                    OsString::from("HOME"),
                    std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/tmp")),
                ),
            ],
            cwd: cwd.to_path_buf(),
        }
    }

    #[test]
    fn exit_code_and_signal_are_reported() {
        let tmp = TempDir::new().unwrap();
        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);

        let exit_script = tmp.path().join("exit42.sh");
        crate::test_support::write_executable(&exit_script, "#!/usr/bin/env sh\nexit 42\n");
        let outcome = run_process(
            script_cmd(&exit_script, &[], tmp.path()),
            b"",
            &tmp.path().join("exit.log"),
            Limits {
                timeout: Duration::from_secs(5),
                stall: Duration::from_secs(5),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || false,
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Exited(42));

        let sig_script = tmp.path().join("sigkill.sh");
        crate::test_support::write_executable(&sig_script, "#!/usr/bin/env sh\nkill -KILL $$\n");
        let outcome = run_process(
            script_cmd(&sig_script, &[], tmp.path()),
            b"",
            &tmp.path().join("sig.log"),
            Limits {
                timeout: Duration::from_secs(5),
                stall: Duration::from_secs(5),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || false,
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Signaled(libc::SIGKILL));
    }

    #[test]
    fn timeout_ends_process_tree_including_setsid_grandchild() {
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let script = tmp.path().join("spawn_setsid.sh");
        crate::test_support::write_executable(
            &script,
            format!(
                "#!/usr/bin/env sh\nsetsid sh -c 'echo $$ > \"{}\"; exec sleep 60' </dev/null >/dev/null 2>&1 &\nsleep 60\n",
                pid_file.display()
            ),
        );

        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);
        let pid_file_clone = pid_file.clone();
        let mut guard = PidGuard(None);

        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            &tmp.path().join("timeout.log"),
            Limits {
                timeout: Duration::from_millis(250),
                stall: Duration::from_secs(10),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || {
                if guard.0.is_none() {
                    if let Ok(raw) = fs::read_to_string(&pid_file_clone) {
                        if let Ok(p) = raw.trim().parse::<i32>() {
                            guard.0 = Some(p);
                        }
                    }
                }
                true
            },
        )
        .unwrap();

        assert_eq!(outcome, Outcome::TimedOut);
        let gpid = guard.0.unwrap_or_else(|| read_pid_file(&pid_file));
        guard.0 = Some(gpid);
        assert!(
            wait_pid_gone(gpid),
            "setsid grandchild {gpid} survived timeout"
        );
    }

    #[test]
    fn stall_ends_process_tree_including_setsid_grandchild() {
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let script = tmp.path().join("stall_setsid.sh");
        crate::test_support::write_executable(
            &script,
            format!(
                "#!/usr/bin/env sh\nsetsid sh -c 'echo $$ > \"{}\"; exec sleep 60' </dev/null >/dev/null 2>&1 &\nsleep 60\n",
                pid_file.display()
            ),
        );

        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);
        let pid_file_clone = pid_file.clone();
        let mut guard = PidGuard(None);

        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            &tmp.path().join("stall.log"),
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_millis(250),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || {
                if guard.0.is_none() {
                    if let Ok(raw) = fs::read_to_string(&pid_file_clone) {
                        if let Ok(p) = raw.trim().parse::<i32>() {
                            guard.0 = Some(p);
                        }
                    }
                }
                false
            },
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Stalled);
        let gpid = guard.0.unwrap_or_else(|| read_pid_file(&pid_file));
        guard.0 = Some(gpid);
        assert!(
            wait_pid_gone(gpid),
            "setsid grandchild {gpid} survived stall"
        );
    }

    #[test]
    fn cancel_ends_process_tree_including_setsid_grandchild() {
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let script = tmp.path().join("cancel_setsid.sh");
        crate::test_support::write_executable(
            &script,
            format!(
                "#!/usr/bin/env sh\nsetsid sh -c 'echo $$ > \"{}\"; exec sleep 60' </dev/null >/dev/null 2>&1 &\nsleep 60\n",
                pid_file.display()
            ),
        );

        let clock = SystemClock::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let pid_file_clone = pid_file.clone();
        let mut guard = PidGuard(None);

        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            &tmp.path().join("cancel.log"),
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || {
                if let Ok(raw) = fs::read_to_string(&pid_file_clone) {
                    if let Ok(p) = raw.trim().parse::<i32>() {
                        guard.0 = Some(p);
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                false
            },
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Cancelled);
        let gpid = guard.0.unwrap_or_else(|| read_pid_file(&pid_file));
        guard.0 = Some(gpid);
        assert!(
            wait_pid_gone(gpid),
            "setsid grandchild {gpid} survived cancel"
        );
    }

    #[test]
    fn silent_process_with_activity_is_not_stalled() {
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("silent.sh");
        crate::test_support::write_executable(
            &script,
            "#!/usr/bin/env sh\n\
             i=0\n\
             while [ \"$i\" -lt 8 ]; do\n\
               sleep 0.05\n\
               i=$((i + 1))\n\
             done\n\
             exit 0\n",
        );

        let clock = StepClock::new(50);
        let cancel = AtomicBool::new(false);
        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            &tmp.path().join("silent.log"),
            Limits {
                timeout: Duration::from_secs(60),
                stall: Duration::from_millis(150),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || true,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Exited(0));
    }

    #[test]
    fn five_mb_single_line_and_invalid_utf8_do_not_crash_and_are_bounded() {
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("megaline.sh");
        crate::test_support::write_executable(
            &script,
            "#!/usr/bin/env sh\n\
             dd if=/dev/zero bs=1048576 count=5 2>/dev/null | tr '\\0' 'A'\n\
             printf '\\xff\\xfe\\xfd\\nsecond-line\\n'\n",
        );

        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);
        let mut lines = Vec::new();
        let log_path = tmp.path().join("mega.log");

        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            &log_path,
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(5),
            },
            &clock,
            &cancel,
            &mut |line| lines.push(line.to_string()),
            &mut || false,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Exited(0));
        assert_eq!(lines.len(), 2, "expected 2 lines, got {}", lines.len());
        assert!(
            lines[0].len() <= MAX_PARSED_LINE_BYTES,
            "parsed line exceeded 1 MiB: {}",
            lines[0].len()
        );
        assert_eq!(lines[1], "second-line");

        let log_meta = fs::metadata(&log_path).unwrap();
        assert!(log_meta.len() > 5 * 1024 * 1024);
    }

    #[test]
    fn large_stdin_does_not_deadlock_when_child_ignores_it() {
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("ignore_stdin.sh");
        crate::test_support::write_executable(&script, "#!/usr/bin/env sh\nexit 0\n");

        let big_stdin = vec![b'x'; 2 * 1024 * 1024];
        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);

        let outcome = run_process(
            script_cmd(&script, &[], tmp.path()),
            &big_stdin,
            &tmp.path().join("stdin.log"),
            Limits {
                timeout: Duration::from_secs(5),
                stall: Duration::from_secs(5),
            },
            &clock,
            &cancel,
            &mut |_| {},
            &mut || false,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Exited(0));
    }

    #[test]
    fn log_cap_truncates_with_note() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("capped.log");
        let mut f = File::create(&log_path).unwrap();
        let mut written = 0;
        let mut truncated = false;

        write_to_log_with_cap(&mut f, b"hello ", &mut written, &mut truncated, 10).unwrap();
        assert!(!truncated);
        write_to_log_with_cap(&mut f, b"world-overflow", &mut written, &mut truncated, 10).unwrap();
        assert!(truncated);
        write_to_log_with_cap(&mut f, b"ignored", &mut written, &mut truncated, 10).unwrap();
        f.flush().unwrap();

        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.starts_with("hello worl"));
        assert!(content.contains("log truncado em 64 MiB"));
        assert!(!content.contains("ignored"));
    }

    fn run_script(tmp: &TempDir, body: &str, log: &Path) -> Result<Outcome> {
        let script = tmp.path().join("script.sh");
        crate::test_support::write_executable(&script, format!("#!/bin/sh\n{body}\n"));
        run_process(
            script_cmd(&script, &[], tmp.path()),
            b"",
            log,
            Limits {
                timeout: Duration::from_secs(20),
                stall: Duration::from_secs(20),
            },
            &SystemClock::new(),
            &AtomicBool::new(false),
            &mut |_| {},
            &mut || false,
        )
    }

    #[test]
    fn background_child_holding_the_output_does_not_delay_the_end() {
        let tmp = TempDir::new().unwrap();
        let start = Instant::now();
        let out = run_script(
            &tmp,
            "sleep 300 & echo $! > bg.pid\necho ok\nexit 7",
            &tmp.path().join("l.log"),
        );
        assert_eq!(out.unwrap(), Outcome::Exited(7));
        assert!(start.elapsed() < Duration::from_secs(15));
        assert!(wait_pid_gone(read_pid_file(&tmp.path().join("bg.pid"))));
    }

    #[test]
    fn a_log_that_cannot_be_written_ends_the_stage_and_its_processes() {
        let tmp = TempDir::new().unwrap();
        let out = run_script(
            &tmp,
            "echo $$ > main.pid\necho cheio\nsleep 300",
            Path::new("/dev/full"),
        );
        assert!(out.is_err());
        assert!(wait_pid_gone(read_pid_file(&tmp.path().join("main.pid"))));
    }

    #[test]
    fn closed_output_does_not_spin_the_monitor() {
        // SAFETY: getrusage writes to a valid struct.
        fn cpu_ms() -> i64 {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut u) };
            let ms = |t: libc::timeval| t.tv_sec * 1000 + t.tv_usec / 1000;
            ms(u.ru_utime) + ms(u.ru_stime)
        }
        let tmp = TempDir::new().unwrap();
        let before = cpu_ms();
        let out = run_script(&tmp, "exec >&- 2>&-\nsleep 2", &tmp.path().join("l.log"));
        assert_eq!(out.unwrap(), Outcome::Exited(0));
        let used = cpu_ms() - before;
        assert!(used < 700, "monitor used {used} ms of CPU in 2 s");
    }

    #[test]
    fn a_reused_pid_is_never_part_of_the_tree() {
        let mut other = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = other.id() as i32;
        let ticks = read_proc_stat(pid).unwrap().start_ticks;
        let our_pid = std::process::id() as i32;
        let tree = |observed_ticks: u64| {
            let mut observed = HashMap::from([(pid, observed_ticks)]);
            collect_tree_pids(&mut TreeContext {
                root_pid: i32::MAX,
                root_start_ticks: Some(u64::MAX),
                our_pid,
                our_sid: unsafe { libc::getsid(0) },
                cwd: Path::new("/nonexistent"),
                observed: &mut observed,
            })
        };
        let (same, reused) = (tree(ticks), tree(ticks + 1));
        let _ = other.kill();
        let _ = other.wait();
        assert_eq!(same, [pid]);
        assert!(reused.is_empty());
    }
}
