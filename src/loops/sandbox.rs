use crate::loops::process::{run_process, Limits, Outcome, SystemClock};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BASE_ALLOWED_ENV: &[&str] = &["PATH", "HOME", "LANG", "TERM"];
pub const AGY_TOKEN_REL: &str = ".gemini/antigravity-cli/antigravity-oauth-token";
pub const AGY_LOGIN_DIR_REL: &str = "loops/agy-login";
pub const AGY_TOKEN_FILE: &str = "antigravity-oauth-token";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyCredentialSource {
    LoopLogin,
    DefaultFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyCredential {
    pub source: AgyCredentialSource,
    pub path: PathBuf,
}

/// The one file each CLI needs from the home directory, always read-only. Measured on 2026-11-23.
const ADAPTER_CREDENTIALS: &[(&str, Option<&str>)] = &[
    ("claude", Some(".claude/.credentials.json")),
    ("agy", Some(AGY_TOKEN_REL)),
    ("opencode", None),
];
const TCHELVIS_HOME_DIRS: &[&str] = &[
    ".config/tchelvis",
    ".local/share/tchelvis",
    ".local/state/tchelvis",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedCommand {
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

pub trait Sandbox {
    fn kind(&self) -> &'static str;
    fn agy_state_dir(&self) -> Option<&Path> {
        None
    }
    fn check(&self, workspace: &Path) -> Result<Vec<String>>;
    fn wrap(&self, argv: &[OsString], workspace: &Path) -> Result<SandboxedCommand>;
    fn wrap_with_extra_env(
        &self,
        argv: &[OsString],
        workspace: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<SandboxedCommand> {
        let mut cmd = self.wrap(argv, workspace)?;
        apply_extra_env(&mut cmd.env, extra_env);
        Ok(cmd)
    }
}

fn is_symlink_component(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn open_dir_nofollow_segments(root: &Path, segments: &[&str]) -> Option<OwnedFd> {
    let root_raw = root.to_str()?;
    let root_c = CString::new(root_raw).ok()?;
    let root_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return None;
    }
    let mut cur = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for seg in segments {
        let c_seg = CString::new(*seg).ok()?;
        let fd = unsafe {
            libc::openat(
                cur.as_raw_fd(),
                c_seg.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        cur = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Some(cur)
}

fn dir_owned_by_current_user(dir_fd: &OwnedFd) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dir_fd.as_raw_fd(), &mut st) } < 0 {
        return false;
    }
    (st.st_mode & libc::S_IFMT) == libc::S_IFDIR && st.st_uid == unsafe { libc::geteuid() }
}

fn regular_non_empty_file_nofollow_owned(dir_fd: BorrowedFd<'_>, name: &str) -> bool {
    let Ok(c_name) = CString::new(name) else {
        return false;
    };
    let fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } < 0 {
        return false;
    }
    (st.st_mode & libc::S_IFMT) == libc::S_IFREG
        && st.st_size > 0
        && st.st_uid == unsafe { libc::geteuid() }
}

/// Single credential resolver for `agy`: loop login under `<state_dir>/loops/agy-login/` first
/// (when `state_dir` is provided, i.e. bwrap), then `<home_dir>/.gemini/antigravity-cli/`.
pub fn resolve_agy_credential(home_dir: &Path, state_dir: Option<&Path>) -> Option<AgyCredential> {
    if let Some(state_dir) = state_dir {
        if !is_symlink_component(state_dir) {
            if let Some(login_dir_fd) =
                open_dir_nofollow_segments(state_dir, &["loops", "agy-login"])
            {
                if dir_owned_by_current_user(&login_dir_fd)
                    && regular_non_empty_file_nofollow_owned(login_dir_fd.as_fd(), AGY_TOKEN_FILE)
                {
                    return Some(AgyCredential {
                        source: AgyCredentialSource::LoopLogin,
                        path: state_dir.join(AGY_LOGIN_DIR_REL).join(AGY_TOKEN_FILE),
                    });
                }
            }
        }
    }

    let path = home_dir.join(AGY_TOKEN_REL);
    fs::metadata(&path)
        .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        .then_some(AgyCredential {
            source: AgyCredentialSource::DefaultFile,
            path,
        })
}

fn apply_extra_env(env: &mut Vec<(OsString, OsString)>, extra_env: &[(&str, &str)]) {
    for &(k, v) in extra_env {
        if let Some(entry) = env.iter_mut().find(|(ek, _)| ek == std::ffi::OsStr::new(k)) {
            entry.1 = OsString::from(v);
        } else {
            env.push((OsString::from(k), OsString::from(v)));
        }
    }
}

fn current_env_pairs() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

fn build_clean_env<K: AsRef<std::ffi::OsStr>, V: AsRef<std::ffi::OsStr>>(
    env_pass: &[String],
    source: &[(K, V)],
) -> Vec<(OsString, OsString)> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut result = Vec::new();

    let lookup = |name: &str| -> Option<OsString> {
        source.iter().find_map(|(k, v)| {
            if k.as_ref() == std::ffi::OsStr::new(name) {
                Some(v.as_ref().to_os_string())
            } else {
                None
            }
        })
    };

    for &name in BASE_ALLOWED_ENV {
        if seen.insert(name.to_string()) {
            if let Some(val) = lookup(name) {
                result.push((OsString::from(name), val));
            }
        }
    }
    for name in env_pass {
        if seen.insert(name.clone()) {
            if let Some(val) = lookup(name) {
                result.push((OsString::from(name), val));
            }
        }
    }
    result
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111) != 0
}

fn find_in_path(bin: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_path_writable_by_current_user(path: &Path) -> bool {
    let Some(s) = path.to_str() else {
        return false;
    };
    let Ok(c) = CString::new(s) else {
        return false;
    };
    // SAFETY: c is a NUL-terminated path string; access(W_OK) queries permissions only.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

fn probe_workspace_writable(workspace: &Path) -> Result<()> {
    if !workspace.is_dir() {
        bail!(
            "workspace não existe ou não é diretório: {}",
            workspace.display()
        );
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_path = workspace.join(format!(
        ".tchelvis-sandbox-probe-{}-{nanos}",
        std::process::id()
    ));
    let token = format!("probe-{nanos}");
    fs::write(&probe_path, &token).with_context(|| {
        format!(
            "falha ao escrever sonda no workspace {}",
            workspace.display()
        )
    })?;
    let read_back = fs::read_to_string(&probe_path);
    let _ = fs::remove_file(&probe_path);
    let read_back = read_back.context("falha ao ler sonda no workspace")?;
    if read_back != token {
        bail!("conteúdo inesperado na sonda do workspace");
    }
    Ok(())
}

pub struct InheritedSandbox {
    pub env_pass: Vec<String>,
    pub root: PathBuf,
    pub test_container_marker: bool,
}

impl InheritedSandbox {
    pub fn new(env_pass: Vec<String>) -> Self {
        #[cfg(debug_assertions)]
        let test_container_marker =
            std::env::var_os("TCHELVIS_TEST_CONTAINER").is_some_and(|v| !v.is_empty());
        #[cfg(not(debug_assertions))]
        let test_container_marker = false;
        Self {
            env_pass,
            root: PathBuf::from("/"),
            test_container_marker,
        }
    }

    pub fn wrap_with_env<K: AsRef<str>, V: AsRef<str>>(
        &self,
        argv: &[OsString],
        workspace: &Path,
        env_source: &[(K, V)],
    ) -> Result<SandboxedCommand> {
        if argv.is_empty() {
            bail!("comando vazio");
        }
        let source: Vec<(&str, &str)> = env_source
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();
        Ok(SandboxedCommand {
            argv: argv.to_vec(),
            env: build_clean_env(&self.env_pass, &source),
            cwd: workspace.to_path_buf(),
        })
    }

    fn detect_shared_writable_mounts(&self) -> Vec<String> {
        let mountinfo_path = self.root.join("proc/self/mountinfo");
        let Ok(raw) = fs::read_to_string(&mountinfo_path) else {
            return Vec::new();
        };
        let mut exposures = Vec::new();
        for line in raw.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let left_fields: Vec<&str> = left.split_whitespace().collect();
            let right_fields: Vec<&str> = right.split_whitespace().collect();
            if left_fields.len() < 6 || right_fields.is_empty() {
                continue;
            }
            let mount_root = left_fields[3];
            let mount_point = left_fields[4];
            let options = left_fields[5];
            let fstype = right_fields[0];

            if !options.split(',').any(|o| o == "rw") {
                continue;
            }
            if mount_point == "/"
                || mount_point.starts_with("/dev")
                || mount_point.starts_with("/proc")
                || mount_point.starts_with("/sys")
                || mount_point.starts_with("/run")
                || mount_point.starts_with("/tmp")
            {
                continue;
            }
            if matches!(
                fstype,
                "proc"
                    | "sysfs"
                    | "tmpfs"
                    | "devtmpfs"
                    | "devpts"
                    | "cgroup"
                    | "cgroup2"
                    | "pstore"
                    | "efivarfs"
                    | "securityfs"
                    | "debugfs"
                    | "tracefs"
                    | "configfs"
                    | "fusectl"
                    | "mqueue"
                    | "hugetlbfs"
                    | "ramfs"
                    | "autofs"
            ) {
                continue;
            }
            let is_external_share = mount_root != "/"
                || options.split(',').any(|o| o == "idmapped")
                || matches!(fstype, "virtiofs" | "9p" | "nfs" | "nfs4" | "cifs");
            if is_external_share {
                exposures.push(format!("montagem gravável compartilhada: {mount_point}"));
            }
        }
        exposures
    }
}

impl Sandbox for InheritedSandbox {
    fn kind(&self) -> &'static str {
        "inherited"
    }

    fn check(&self, workspace: &Path) -> Result<Vec<String>> {
        let markers = [
            self.root.join("run/systemd/container"),
            self.root.join(".dockerenv"),
            self.root.join("run/.containerenv"),
        ];
        #[cfg(debug_assertions)]
        let marker_ok = self.test_container_marker || markers.iter().any(|p| p.exists());
        #[cfg(not(debug_assertions))]
        let marker_ok = markers.iter().any(|p| p.exists());
        if !marker_ok {
            bail!(
                "sandbox 'inherited' exige execução dentro de um container (/run/systemd/container, /.dockerenv ou /run/.containerenv não encontrado)"
            );
        }

        probe_workspace_writable(workspace)?;

        let mut exposures = Vec::new();

        for rel in [
            "var/run/docker.sock",
            "run/docker.sock",
            "var/lib/incus/unix.socket",
            "run/incus/unix.socket",
        ] {
            let p = self.root.join(rel);
            if fs::symlink_metadata(&p).is_ok() && is_path_writable_by_current_user(&p) {
                exposures.push(format!("socket gravável acessível: /{rel}"));
            }
        }

        if self.root == Path::new("/") {
            if let Some(sudo_bin) = find_in_path("sudo", std::env::var_os("PATH").as_deref()) {
                let ok = Command::new(sudo_bin)
                    .args(["-n", "true"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    exposures.push("sudo sem senha disponível (sudo -n true)".to_string());
                }
            }
            if find_in_path("hc", std::env::var_os("PATH").as_deref()).is_some() {
                exposures.push("hc disponível no PATH (acesso a outros terminais)".to_string());
            }
        }

        exposures.extend(self.detect_shared_writable_mounts());
        Ok(exposures)
    }

    fn wrap(&self, argv: &[OsString], workspace: &Path) -> Result<SandboxedCommand> {
        if argv.is_empty() {
            bail!("comando vazio");
        }
        let env_pairs = current_env_pairs();
        Ok(SandboxedCommand {
            argv: argv.to_vec(),
            env: build_clean_env(&self.env_pass, &env_pairs),
            cwd: workspace.to_path_buf(),
        })
    }
}

pub struct BwrapSandbox {
    pub home_ro: Vec<String>,
    pub env_pass: Vec<String>,
    pub state_dir: PathBuf,
}

impl BwrapSandbox {
    pub fn new(home_ro: Vec<String>, env_pass: Vec<String>, state_dir: PathBuf) -> Self {
        Self {
            home_ro,
            env_pass,
            state_dir,
        }
    }

    fn is_under_tchelvis_home(home: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(home) else {
            return false;
        };
        for own in TCHELVIS_HOME_DIRS {
            let own = Path::new(own);
            if rel.starts_with(own) || own.starts_with(rel) {
                return true;
            }
        }
        false
    }

    /// What the CLI named by `program` needs from the home directory, as `(source, destination)`
    /// read-only binds: its own executable and its credential, never the directories around them.
    fn adapter_binds(
        program: &std::ffi::OsStr,
        home: &Path,
        path_var: Option<&std::ffi::OsStr>,
        state_dir: &Path,
    ) -> Vec<(PathBuf, PathBuf)> {
        let Some((name, credential)) = ADAPTER_CREDENTIALS
            .iter()
            .find(|(name, _)| program == std::ffi::OsStr::new(name))
        else {
            return Vec::new();
        };
        let mut binds = Vec::new();
        if let Some(bin) = find_in_path(name, path_var) {
            let real = fs::canonicalize(&bin).unwrap_or_else(|_| bin.clone());
            let path_dirs: Vec<PathBuf> = path_var
                .map(|p| std::env::split_paths(p).collect())
                .unwrap_or_default();
            if real != bin && real.starts_with(home) && !Self::is_under_tchelvis_home(home, &real) {
                // The directory of the real executable may hold files it loads; a directory of the
                // PATH holds unrelated programs, and one above tchelvis' own holds its state.
                match real.parent() {
                    Some(dir)
                        if !path_dirs.iter().any(|p| p == dir)
                            && !Self::is_under_tchelvis_home(home, dir) =>
                    {
                        binds.push((dir.to_path_buf(), dir.to_path_buf()));
                    }
                    _ => binds.push((real.clone(), real.clone())),
                }
            }
            if bin.starts_with(home) {
                binds.push((real, bin));
            }
        }
        if *name == "agy" {
            if let Some(cred) = resolve_agy_credential(home, Some(state_dir)) {
                binds.push((cred.path, home.join(AGY_TOKEN_REL)));
            }
        } else if let Some(rel) = credential {
            let file = home.join(rel);
            if file.is_file() {
                binds.push((file.clone(), file));
            }
        }
        binds
    }

    fn wrap_clean(
        &self,
        argv: &[OsString],
        workspace: &Path,
        clean_env: Vec<(OsString, OsString)>,
    ) -> Result<SandboxedCommand> {
        if argv.is_empty() {
            bail!("comando vazio");
        }
        let home = clean_env
            .iter()
            .find(|(k, _)| k == std::ffi::OsStr::new("HOME"))
            .map(|(_, v)| PathBuf::from(v))
            .filter(|p| !p.as_os_str().is_empty())
            .context("variável de ambiente HOME ausente para sandbox bwrap")?;

        let path_var = clean_env
            .iter()
            .find(|(k, _)| k == std::ffi::OsStr::new("PATH"))
            .map(|(_, v)| v.as_os_str());

        let mut binds: Vec<(PathBuf, PathBuf)> = Vec::new();
        for rel in &self.home_ro {
            let full = home.join(rel);
            if let Ok(real) = fs::canonicalize(&full) {
                if Self::is_under_tchelvis_home(&home, &real) {
                    bail!("'{rel}' em home_ro aponta para os arquivos do tchelvis");
                }
                binds.push((full.clone(), full));
            }
        }
        binds.extend(Self::adapter_binds(
            &argv[0],
            &home,
            path_var,
            &self.state_dir,
        ));

        let mut wrapped = vec![
            OsString::from("bwrap"),
            OsString::from("--die-with-parent"),
            OsString::from("--unshare-pid"),
            OsString::from("--unshare-ipc"),
            OsString::from("--unshare-uts"),
            OsString::from("--new-session"),
            OsString::from("--ro-bind"),
            OsString::from("/"),
            OsString::from("/"),
            OsString::from("--dev"),
            OsString::from("/dev"),
            OsString::from("--proc"),
            OsString::from("/proc"),
            OsString::from("--tmpfs"),
            OsString::from("/run"),
            OsString::from("--tmpfs"),
            OsString::from("/tmp"),
            OsString::from("--ro-bind-try"),
            OsString::from("/run/systemd/resolve"),
            OsString::from("/run/systemd/resolve"),
            OsString::from("--tmpfs"),
            home.as_os_str().to_os_string(),
        ];

        for (src, dst) in &binds {
            wrapped.push(OsString::from("--ro-bind"));
            wrapped.push(src.as_os_str().to_os_string());
            wrapped.push(dst.as_os_str().to_os_string());
        }

        wrapped.push(OsString::from("--bind"));
        wrapped.push(workspace.as_os_str().to_os_string());
        wrapped.push(workspace.as_os_str().to_os_string());

        // The environment is already clean (`run_process` starts from an empty one) and is inherited:
        // `--setenv` would show its values to every process that can read the command line.
        wrapped.push(OsString::from("--chdir"));
        wrapped.push(workspace.as_os_str().to_os_string());
        wrapped.push(OsString::from("--"));
        wrapped.extend(argv.iter().cloned());

        Ok(SandboxedCommand {
            argv: wrapped,
            env: clean_env,
            cwd: workspace.to_path_buf(),
        })
    }

    pub fn wrap_with_env<K: AsRef<str>, V: AsRef<str>>(
        &self,
        argv: &[OsString],
        workspace: &Path,
        env_source: &[(K, V)],
    ) -> Result<SandboxedCommand> {
        let source: Vec<(&str, &str)> = env_source
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();
        let clean_env = build_clean_env(&self.env_pass, &source);
        self.wrap_clean(argv, workspace, clean_env)
    }

    fn check_clean(
        &self,
        workspace: &Path,
        clean_env: Vec<(OsString, OsString)>,
    ) -> Result<Vec<String>> {
        if !workspace.is_dir() {
            bail!(
                "workspace não existe ou não é diretório: {}",
                workspace.display()
            );
        }

        let home = clean_env
            .iter()
            .find(|(k, _)| k == std::ffi::OsStr::new("HOME"))
            .map(|(_, v)| PathBuf::from(v))
            .filter(|p| !p.as_os_str().is_empty())
            .context("variável de ambiente HOME ausente para sandbox bwrap")?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let probe_token = format!("tchelvis-bwrap-probe-{pid}-{nanos}");
        let sentinel_secret = format!("tchelvis-sentinel-secret-{pid}-{nanos}");

        let probe_in = workspace.join(format!(".tchelvis-bwrap-probe-in-{pid}-{nanos}"));
        let probe_out = workspace.join(format!(".tchelvis-bwrap-probe-out-{pid}-{nanos}"));
        let probe_log = workspace.join(format!(".tchelvis-bwrap-probe-log-{pid}-{nanos}"));

        struct CleanupFiles(Vec<PathBuf>);
        impl Drop for CleanupFiles {
            fn drop(&mut self) {
                for p in &self.0 {
                    let _ = fs::remove_file(p);
                }
            }
        }
        let sentinel_dir = if home.is_dir() {
            home.clone()
        } else {
            std::env::temp_dir()
        };
        let sentinel_path = sentinel_dir.join(format!(".tchelvis-sentinel-{pid}-{nanos}"));
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut sentinel = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&sentinel_path)
                .context("falha ao criar arquivo sentinela fora do workspace")?;
            sentinel.write_all(sentinel_secret.as_bytes())?;
            sentinel.flush()?;
        }
        let _cleanup = CleanupFiles(vec![
            probe_in.clone(),
            probe_out.clone(),
            probe_log.clone(),
            sentinel_path.clone(),
        ]);

        fs::write(&probe_in, &probe_token)
            .context("falha ao gravar arquivo de sonda no workspace")?;

        let script = "val=$(cat \"$1\") || exit 11\n\
             printf '%s' \"$val\" > \"$2\" || exit 12\n\
             if cat \"$3\" 2>/dev/null; then exit 13; fi\n\
             echo tampered >> \"$3\" 2>/dev/null || true\n\
             printf '%s' \"$val\"\n";

        let wrapped = self.wrap_clean(
            &[
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("bwrap-probe"),
                probe_in.as_os_str().to_os_string(),
                probe_out.as_os_str().to_os_string(),
                sentinel_path.as_os_str().to_os_string(),
            ],
            workspace,
            clean_env,
        )?;

        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);
        let mut lines = Vec::new();
        let outcome = run_process(
            wrapped,
            b"",
            &probe_log,
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
            &clock,
            &cancel,
            &mut |l| lines.push(l.to_string()),
            &mut || false,
        )
        .context("falha ao executar sonda do sandbox bwrap (bubblewrap instalado e funcional?)")?;

        if outcome != Outcome::Exited(0) {
            let log_tail = fs::read_to_string(&probe_log).unwrap_or_default();
            bail!(
                "sonda do sandbox bwrap falhou ({outcome:?}): {}",
                log_tail.trim()
            );
        }

        let joined = lines.join("\n");
        if !joined.contains(&probe_token) {
            bail!("sonda bwrap não conseguiu ler o arquivo de entrada no workspace");
        }
        if joined.contains(&sentinel_secret) {
            bail!("sonda bwrap vazou conteúdo do arquivo sentinela externo");
        }
        let written_out = fs::read_to_string(&probe_out)
            .context("sonda bwrap não conseguiu gravar no workspace")?;
        if written_out != probe_token {
            bail!("sonda bwrap gravou conteúdo divergente no workspace");
        }
        let sentinel_after = fs::read_to_string(&sentinel_path)
            .context("falha ao verificar arquivo sentinela após sonda bwrap")?;
        if sentinel_after != sentinel_secret {
            bail!("sonda bwrap alterou arquivo sentinela fora do workspace");
        }

        Ok(vec![
            "rede compartilhada com o host/container".to_string(),
            "serviços em localhost e sockets abstratos compartilhados".to_string(),
        ])
    }

    pub fn check_with_env<K: AsRef<str>, V: AsRef<str>>(
        &self,
        workspace: &Path,
        env_source: &[(K, V)],
    ) -> Result<Vec<String>> {
        let source: Vec<(&str, &str)> = env_source
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();
        let clean_env = build_clean_env(&self.env_pass, &source);
        self.check_clean(workspace, clean_env)
    }
}

impl Sandbox for BwrapSandbox {
    fn kind(&self) -> &'static str {
        "bwrap"
    }

    fn agy_state_dir(&self) -> Option<&Path> {
        Some(&self.state_dir)
    }

    fn check(&self, workspace: &Path) -> Result<Vec<String>> {
        let env_pairs = current_env_pairs();
        let clean_env = build_clean_env(&self.env_pass, &env_pairs);
        self.check_clean(workspace, clean_env)
    }

    fn wrap(&self, argv: &[OsString], workspace: &Path) -> Result<SandboxedCommand> {
        let env_pairs = current_env_pairs();
        let clean_env = build_clean_env(&self.env_pass, &env_pairs);
        self.wrap_clean(argv, workspace, clean_env)
    }

    fn wrap_with_extra_env(
        &self,
        argv: &[OsString],
        workspace: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<SandboxedCommand> {
        let env_pairs = current_env_pairs();
        let mut clean_env = build_clean_env(&self.env_pass, &env_pairs);
        apply_extra_env(&mut clean_env, extra_env);
        self.wrap_clean(argv, workspace, clean_env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::process::{run_process, Limits, Outcome, SystemClock};
    use std::collections::BTreeSet;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tempfile::TempDir;

    fn bwrap_available() -> bool {
        Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--", "/bin/true"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn bwrap_argv_is_exact() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".cargo")).unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        let versions = home.join(".local/share/claude/versions");
        fs::create_dir_all(&versions).unwrap();
        crate::test_support::write_executable(versions.join("1.0"), "#!/bin/sh\n");
        std::os::unix::fs::symlink(versions.join("1.0"), home.join(".local/bin/claude")).unwrap();
        fs::create_dir_all(&ws).unwrap();
        let path_var = format!("{}:/usr/bin:/bin", home.join(".local/bin").display());

        let env_map = [
            ("PATH", path_var.as_str()),
            ("HOME", home.to_str().unwrap()),
            ("LANG", "C.UTF-8"),
            ("TERM", "xterm-256color"),
            ("CUSTOM_KEY", "secret-val"),
            ("IGNORED_SECRET", "must-not-leak"),
        ];

        let sb = BwrapSandbox::new(
            vec![".cargo".into(), ".missing_ro".into()],
            vec!["CUSTOM_KEY".into()],
            tmp.path().join("state"),
        );
        let cmd = sb
            .wrap_with_env(
                &[OsString::from("claude"), OsString::from("--version")],
                &ws,
                &env_map,
            )
            .unwrap();

        let expected_argv: Vec<OsString> = vec![
            "bwrap".into(),
            "--die-with-parent".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--unshare-uts".into(),
            "--new-session".into(),
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--tmpfs".into(),
            "/run".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--ro-bind-try".into(),
            "/run/systemd/resolve".into(),
            "/run/systemd/resolve".into(),
            "--tmpfs".into(),
            home.as_os_str().to_os_string(),
            "--ro-bind".into(),
            home.join(".cargo").into_os_string(),
            home.join(".cargo").into_os_string(),
            "--ro-bind".into(),
            versions.clone().into_os_string(),
            versions.clone().into_os_string(),
            "--ro-bind".into(),
            versions.join("1.0").into_os_string(),
            home.join(".local/bin/claude").into_os_string(),
            "--ro-bind".into(),
            home.join(".claude/.credentials.json").into_os_string(),
            home.join(".claude/.credentials.json").into_os_string(),
            "--bind".into(),
            ws.as_os_str().to_os_string(),
            ws.as_os_str().to_os_string(),
            "--chdir".into(),
            ws.as_os_str().to_os_string(),
            "--".into(),
            "claude".into(),
            "--version".into(),
        ];

        assert_eq!(cmd.argv, expected_argv);
        assert_eq!(cmd.cwd, ws);
        let expected_env = [
            ("PATH", path_var.as_str()),
            ("HOME", home.to_str().unwrap()),
            ("LANG", "C.UTF-8"),
            ("TERM", "xterm-256color"),
            ("CUSTOM_KEY", "secret-val"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        assert_eq!(cmd.env, expected_env);
        assert!(!cmd
            .argv
            .iter()
            .any(|a| a.to_string_lossy().contains("secret-val")));
    }

    #[test]
    fn environment_contains_only_allowed_names() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&ws).unwrap();

        let env_map = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", home.to_str().unwrap()),
            ("LANG", "C.UTF-8"),
            ("TERM", "xterm"),
            ("ALLOWED_API_KEY", "abc"),
            ("AWS_SECRET_ACCESS_KEY", "nope"),
            ("HC_SOCKET", "/tmp/hc.sock"),
        ];

        let allowed: BTreeSet<&str> = ["PATH", "HOME", "LANG", "TERM", "ALLOWED_API_KEY"]
            .into_iter()
            .collect();

        let inherited = InheritedSandbox::new(vec!["ALLOWED_API_KEY".into()]);
        let cmd_inh = inherited
            .wrap_with_env(&[OsString::from("/bin/true")], &ws, &env_map)
            .unwrap();
        let names_inh: BTreeSet<String> = cmd_inh
            .env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names_inh,
            allowed
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );

        let bwrap = BwrapSandbox::new(
            vec![],
            vec!["ALLOWED_API_KEY".into()],
            tmp.path().join("state"),
        );
        let cmd_bw = bwrap
            .wrap_with_env(&[OsString::from("/bin/true")], &ws, &env_map)
            .unwrap();
        let names_bw: BTreeSet<String> = cmd_bw
            .env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names_bw,
            allowed
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn bwrap_check_blocks_outside_reads_and_writes() {
        if !bwrap_available() {
            println!("SKIPPED: bwrap unavailable");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&ws).unwrap();

        let sentinel = home.join("sentinel.secret");
        fs::write(&sentinel, "do-not-read-me").unwrap();

        let env_map = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", home.to_str().unwrap()),
            ("LANG", "C.UTF-8"),
            ("TERM", "xterm"),
        ];

        let sb = BwrapSandbox::new(vec![], vec![], tmp.path().join("state"));
        let exposures = sb.check_with_env(&ws, &env_map).unwrap();
        assert!(exposures
            .iter()
            .any(|e| e.contains("rede") || e.contains("localhost")));

        let cmd = sb
            .wrap_with_env(
                &[
                    OsString::from("/bin/sh"),
                    OsString::from("-c"),
                    OsString::from(format!(
                        "if cat '{}' 2>/dev/null; then echo LEAKED; fi; echo hacked > '{}' 2>/dev/null || true; echo inside > '{}/ok.txt'",
                        sentinel.display(),
                        sentinel.display(),
                        ws.display()
                    )),
                ],
                &ws,
                &env_map,
            )
            .unwrap();

        let mut lines = Vec::new();
        let clock = SystemClock::new();
        let cancel = AtomicBool::new(false);
        let outcome = run_process(
            cmd,
            b"",
            &tmp.path().join("bwrap.log"),
            Limits {
                timeout: Duration::from_secs(5),
                stall: Duration::from_secs(5),
            },
            &clock,
            &cancel,
            &mut |l| lines.push(l.to_string()),
            &mut || false,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Exited(0));
        assert!(!lines
            .iter()
            .any(|l| l.contains("do-not-read-me") || l.contains("LEAKED")));
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "do-not-read-me");
        assert_eq!(fs::read_to_string(ws.join("ok.txt")).unwrap(), "inside\n");
    }

    #[test]
    fn inherited_requires_a_container() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("rootfs");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&ws).unwrap();

        let mut sb = InheritedSandbox::new(vec![]);
        sb.root = root.clone();

        // No marker file -> must error
        assert!(sb.check(&ws).is_err());

        // Creating /.dockerenv enables inherited
        fs::write(root.join(".dockerenv"), "").unwrap();
        let exposures = sb.check(&ws).unwrap();
        assert!(exposures.is_empty(), "unexpected exposures: {exposures:?}");

        // Inject a shared writable mount in fake /proc/self/mountinfo -> detected as exposure
        fs::create_dir_all(root.join("proc/self")).unwrap();
        fs::write(
            root.join("proc/self/mountinfo"),
            "1229 1210 0:37 /@home/host/shared /home/user/shared rw,noatime,idmapped - btrfs /dev/nvme0n1p2 rw\n",
        )
        .unwrap();
        let exposures = sb.check(&ws).unwrap();
        assert!(
            exposures.iter().any(|e| e.contains("/home/user/shared")),
            "expected shared mount exposure, got: {exposures:?}"
        );
    }

    #[test]
    fn a_stage_sees_only_its_own_cli_and_credential_and_leaves_nothing_behind() {
        if !bwrap_available() {
            println!("SKIPPED: bwrap unavailable");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (home, ws) = (tmp.path().join("home"), tmp.path().join("ws"));
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(home.join(".claude/.credentials.json"), "cred").unwrap();
        fs::write(home.join(".claude/settings.json"), "original").unwrap();
        fs::write(
            home.join(".gemini/antigravity-cli/antigravity-oauth-token"),
            "outro",
        )
        .unwrap();
        crate::test_support::write_executable(bin.join("hc"), "#!/bin/sh\n");
        crate::test_support::write_executable(
            bin.join("claude"),
            "#!/bin/sh\ncd \"$HOME\" && find . -type f | sort\n\
             echo hook > .claude/settings.json\necho x > .bashrc\n\
             echo x >> .claude/.credentials.json\necho x >> .local/bin/claude\nexit 0\n",
        );
        let path_var = format!("{}:/usr/bin:/bin", bin.display());
        let cmd = BwrapSandbox::new(vec![], vec![], tmp.path().join("state"))
            .wrap_with_env(
                &[OsString::from("claude")],
                &ws,
                &[
                    ("PATH", path_var.as_str()),
                    ("HOME", home.to_str().unwrap()),
                ],
            )
            .unwrap();
        let mut lines = Vec::new();
        run_process(
            cmd,
            b"",
            &tmp.path().join("l.log"),
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
            &SystemClock::new(),
            &AtomicBool::new(false),
            &mut |l| lines.push(l.to_string()),
            &mut || false,
        )
        .unwrap();
        assert_eq!(
            lines,
            ["./.claude/.credentials.json", "./.local/bin/claude"]
        );
        let read = |rel: &str| fs::read_to_string(home.join(rel)).unwrap();
        assert_eq!(read(".claude/settings.json"), "original");
        assert_eq!(read(".claude/.credentials.json"), "cred");
        assert!(!read(".local/bin/claude").ends_with("x\n"));
        assert!(!home.join(".bashrc").exists());
    }

    #[test]
    fn wrap_with_extra_env_passes_key_only_through_process_env_not_bwrap_argv() {
        let sb = BwrapSandbox::new(vec![], vec![], PathBuf::from("/tmp/state"));
        let cmd = sb
            .wrap_with_extra_env(
                &[OsString::from("opencode"), OsString::from("run")],
                Path::new("/tmp/ws"),
                &[
                    ("HOME", "/tmp/home"),
                    ("PATH", "/usr/bin"),
                    ("OPENCODE_API_KEY", "sk-opencode-secret-999"),
                ],
            )
            .unwrap();
        let argv_joined: String = cmd
            .argv
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !argv_joined.contains("sk-opencode-secret-999"),
            "secret leaked into bwrap argv: {argv_joined}"
        );
        assert!(
            !cmd.argv.iter().any(|a| a == "--setenv"),
            "bwrap must never pass secrets via --setenv"
        );
        assert!(cmd
            .env
            .iter()
            .any(|(k, v)| k == "OPENCODE_API_KEY" && v == "sk-opencode-secret-999"));
    }

    #[test]
    fn inherited_sandbox_debug_test_marker_allows_container_detection() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&ws).unwrap();

        let sb = InheritedSandbox {
            env_pass: vec![],
            root,
            test_container_marker: true,
        };
        if cfg!(debug_assertions) {
            assert!(sb.check(&ws).is_ok());
        } else {
            assert!(sb.check(&ws).is_err());
        }
    }

    #[test]
    fn a_cli_next_to_the_tchelvis_directories_exposes_only_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let state_dir = tmp.path().join("state");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        for parent in [".local/share", ".local/state", ".config", ".local"] {
            fs::create_dir_all(home.join(parent)).unwrap();
            let real = home.join(parent).join("claude-bin");
            crate::test_support::write_executable(&real, "#!/bin/sh\n");
            let link = bin.join("claude");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let binds = BwrapSandbox::adapter_binds(
                std::ffi::OsStr::new("claude"),
                &home,
                Some(bin.as_os_str()),
                &state_dir,
            );
            assert_eq!(
                binds,
                vec![(real.clone(), real.clone()), (real.clone(), link.clone())],
                "{parent}"
            );
        }
    }

    #[test]
    fn resolve_agy_credential_enforces_priority_empty_symlinks_and_inherited() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = home.join(".local/state/tchelvis");
        let loop_dir = state.join(AGY_LOGIN_DIR_REL);
        let loop_tok = loop_dir.join(AGY_TOKEN_FILE);
        let default_dir = home.join(".gemini/antigravity-cli");
        let default_tok = default_dir.join(AGY_TOKEN_FILE);

        assert_eq!(resolve_agy_credential(&home, Some(&state)), None);

        // Only default present -> DefaultFile
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(&default_tok, "default-token\n").unwrap();
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::DefaultFile,
                path: default_tok.clone(),
            })
        );

        // Both present -> LoopLogin wins in bwrap (Some(state)), DefaultFile in inherited (None)
        fs::create_dir_all(&loop_dir).unwrap();
        fs::write(&loop_tok, "loop-token\n").unwrap();
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::LoopLogin,
                path: loop_tok.clone(),
            })
        );
        assert_eq!(
            resolve_agy_credential(&home, None),
            Some(AgyCredential {
                source: AgyCredentialSource::DefaultFile,
                path: default_tok.clone(),
            })
        );

        // Empty loop token falls back to default; empty default with no loop -> None
        fs::write(&loop_tok, "").unwrap();
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::DefaultFile,
                path: default_tok.clone(),
            })
        );
        fs::write(&default_tok, "").unwrap();
        assert_eq!(resolve_agy_credential(&home, Some(&state)), None);
        assert_eq!(resolve_agy_credential(&home, None), None);

        // Restoring default and removing loop -> DefaultFile; symlinked loop token is ignored
        fs::write(&default_tok, "default-token\n").unwrap();
        let decoy_file = tmp.path().join("decoy-token");
        fs::write(&decoy_file, "decoy\n").unwrap();
        fs::remove_file(&loop_tok).unwrap();
        std::os::unix::fs::symlink(&decoy_file, &loop_tok).unwrap();
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::DefaultFile,
                path: default_tok.clone(),
            })
        );

        // Symlinked agy-login dir is ignored
        fs::remove_dir_all(&loop_dir).unwrap();
        let decoy_dir = tmp.path().join("decoy-dir");
        fs::create_dir_all(&decoy_dir).unwrap();
        fs::write(decoy_dir.join(AGY_TOKEN_FILE), "decoy\n").unwrap();
        std::os::unix::fs::symlink(&decoy_dir, &loop_dir).unwrap();
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::DefaultFile,
                path: default_tok.clone(),
            })
        );

        // Only loop login (no default) -> inherited ignores loop login
        fs::remove_file(&loop_dir).unwrap();
        fs::create_dir_all(&loop_dir).unwrap();
        fs::write(&loop_tok, "loop-token\n").unwrap();
        fs::remove_file(&default_tok).unwrap();
        assert_eq!(resolve_agy_credential(&home, None), None);
        assert_eq!(
            resolve_agy_credential(&home, Some(&state)),
            Some(AgyCredential {
                source: AgyCredentialSource::LoopLogin,
                path: loop_tok.clone(),
            })
        );

        // The default file is the user's own layout: links in it are followed, as always.
        let default = Some(AgyCredential {
            source: AgyCredentialSource::DefaultFile,
            path: default_tok.clone(),
        });
        fs::remove_dir_all(&loop_dir).unwrap();
        std::os::unix::fs::symlink(&decoy_file, &default_tok).unwrap();
        assert_eq!(resolve_agy_credential(&home, Some(&state)), default);
        fs::remove_file(&default_tok).unwrap();
        assert_eq!(resolve_agy_credential(&home, Some(&state)), None);

        fs::remove_dir_all(&default_dir).unwrap();
        std::os::unix::fs::symlink(&decoy_dir, &default_dir).unwrap();
        assert_eq!(resolve_agy_credential(&home, Some(&state)), default);

        fs::remove_dir_all(home.join(".gemini")).unwrap();
        let decoy_gemini = tmp.path().join("decoy-gemini");
        fs::create_dir_all(decoy_gemini.join("antigravity-cli")).unwrap();
        fs::write(
            decoy_gemini.join("antigravity-cli").join(AGY_TOKEN_FILE),
            "decoy\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&decoy_gemini, home.join(".gemini")).unwrap();
        assert_eq!(resolve_agy_credential(&home, Some(&state)), default);
    }

    #[test]
    fn bwrap_mounts_only_chosen_agy_credential_readonly_and_hides_state_and_siblings() {
        if !bwrap_available() {
            println!("SKIPPED: bwrap unavailable");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (home, ws) = (tmp.path().join("home"), tmp.path().join("ws"));
        let state_dir = home.join(".local/state/tchelvis");
        let loop_dir = state_dir.join(AGY_LOGIN_DIR_REL);
        let loop_tok = loop_dir.join(AGY_TOKEN_FILE);
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&loop_dir).unwrap();
        fs::create_dir_all(loop_dir.join("logs")).unwrap();
        fs::create_dir_all(loop_dir.join("conversations")).unwrap();
        fs::create_dir_all(&ws).unwrap();

        // Populate loop login + sibling state/logs + default file (different content)
        fs::write(&loop_tok, "loop-secret-token\n").unwrap();
        fs::write(loop_dir.join("logs/agy.log"), "private-log").unwrap();
        fs::write(loop_dir.join("conversations/c1.json"), "private-conv").unwrap();
        fs::write(state_dir.join("other-state.txt"), "tchelvis-state").unwrap();
        fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        fs::write(home.join(AGY_TOKEN_REL), "default-token\n").unwrap();

        crate::test_support::write_executable(
            bin.join("agy"),
            "#!/bin/sh\ncd \"$HOME\" && find . -type f | sort\n\
             echo TOKEN=$(cat .gemini/antigravity-cli/antigravity-oauth-token)\n\
             if echo tampered >> .gemini/antigravity-cli/antigravity-oauth-token 2>/dev/null; then echo WROTE; fi\n\
             exit 0\n",
        );

        let path_var = format!("{}:/usr/bin:/bin", bin.display());
        let cmd = BwrapSandbox::new(vec![], vec![], state_dir.clone())
            .wrap_with_env(
                &[OsString::from("agy")],
                &ws,
                &[
                    ("PATH", path_var.as_str()),
                    ("HOME", home.to_str().unwrap()),
                ],
            )
            .unwrap();
        let mut lines = Vec::new();
        let outcome = run_process(
            cmd,
            b"",
            &tmp.path().join("agy-bwrap.log"),
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
            &SystemClock::new(),
            &AtomicBool::new(false),
            &mut |l| lines.push(l.to_string()),
            &mut || false,
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Exited(0));
        assert_eq!(
            lines,
            [
                "./.gemini/antigravity-cli/antigravity-oauth-token",
                "./.local/bin/agy",
                "TOKEN=loop-secret-token",
            ]
        );
        assert_eq!(
            fs::read_to_string(&loop_tok).unwrap(),
            "loop-secret-token\n"
        );

        // Even after deleting ~/.gemini on host, bwrap still mounts loop-token at the exact destination
        fs::remove_dir_all(home.join(".gemini")).unwrap();
        let cmd2 = BwrapSandbox::new(vec![], vec![], state_dir)
            .wrap_with_env(
                &[OsString::from("agy")],
                &ws,
                &[
                    ("PATH", path_var.as_str()),
                    ("HOME", home.to_str().unwrap()),
                ],
            )
            .unwrap();
        let mut lines2 = Vec::new();
        let outcome2 = run_process(
            cmd2,
            b"",
            &tmp.path().join("agy-bwrap2.log"),
            Limits {
                timeout: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
            &SystemClock::new(),
            &AtomicBool::new(false),
            &mut |l| lines2.push(l.to_string()),
            &mut || false,
        )
        .unwrap();
        assert_eq!(outcome2, Outcome::Exited(0));
        assert_eq!(
            lines2,
            [
                "./.gemini/antigravity-cli/antigravity-oauth-token",
                "./.local/bin/agy",
                "TOKEN=loop-secret-token",
            ]
        );
    }
}
