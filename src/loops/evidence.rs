use crate::loops::models::ModelChoice;
use crate::loops::workflow::Provider;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const MAX_EVIDENCE_FILE_BYTES: usize = 4 * 1024 * 1024;
const LOCK_FILE_NAME: &str = ".model_evidence.lock";
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(20);
static NEXT_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliIdentity {
    pub executable: String,
    pub account: String,
    pub sandbox: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountIdentityInput<'a> {
    ClaudeKnown {
        org_id: Option<&'a str>,
        org_name: Option<&'a str>,
        email: Option<&'a str>,
    },
    OpencodeKey(Option<&'a str>),
    AgyTokenPresent(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub identity: CliIdentity,
    pub recorded_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvidenceFile {
    schema_version: u32,
    salt: String,
    #[serde(default)]
    entries: Vec<EvidenceEntry>,
}

pub struct EvidenceStore {
    path: PathBuf,
    salt: [u8; 32],
    entries: Vec<EvidenceEntry>,
    warning: Option<String>,
    loaded_from_disk: bool,
}

fn random_salt() -> [u8; 32] {
    let mut salt = [0u8; 32];
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(&mut salt).is_ok() {
            return salt;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    salt.copy_from_slice(&hasher.finalize());
    salt
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (idx, chunk) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)? as u8;
        let lo = (chunk[1] as char).to_digit(16)? as u8;
        out[idx] = (hi << 4) | lo;
    }
    Some(out)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha256::digest(key);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

fn open_dir_nofollow(dir: &Path) -> std::result::Result<Option<OwnedFd>, String> {
    let Some(raw) = dir.to_str() else {
        return Err("caminho do diretório de evidências não é UTF-8".to_string());
    };
    let Ok(c_path) = CString::new(raw) else {
        return Err("caminho do diretório de evidências inválido".to_string());
    };
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        if err.raw_os_error() == Some(libc::ELOOP) {
            return Err("diretório de evidências é um link simbólico e foi ignorado".to_string());
        }
        if err.raw_os_error() == Some(libc::ENOTDIR) {
            return Err("caminho pai de evidências não é um diretório".to_string());
        }
        return Err(format!("falha ao abrir diretório de evidências: {err}"));
    }
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn read_evidence_at(
    dir_fd: BorrowedFd<'_>,
    file_name: &str,
) -> std::result::Result<Option<EvidenceFile>, String> {
    let Ok(c_name) = CString::new(file_name) else {
        return Err("nome de arquivo de evidências inválido".to_string());
    };
    let fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        if err.raw_os_error() == Some(libc::ELOOP) {
            return Err("arquivo de evidências é um link simbólico e foi ignorado".to_string());
        }
        return Err(format!("falha ao abrir arquivo de evidências: {err}"));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } < 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("falha ao inspecionar arquivo de evidências: {err}"));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err("arquivo de evidências não é um arquivo regular".to_string());
    }
    if st.st_size < 0 || st.st_size as usize > MAX_EVIDENCE_FILE_BYTES {
        return Err("arquivo de evidências excede o tamanho máximo".to_string());
    }

    let file = File::from(owned);
    let mut buf = Vec::with_capacity(st.st_size as usize);
    if let Err(err) = file
        .take((MAX_EVIDENCE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut buf)
    {
        return Err(format!("falha ao ler arquivo de evidências: {err}"));
    }
    if buf.len() > MAX_EVIDENCE_FILE_BYTES {
        return Err("arquivo de evidências excede o tamanho máximo".to_string());
    }
    let parsed: EvidenceFile = serde_json::from_slice(&buf)
        .map_err(|_| "arquivo de evidências corrompido (JSON inválido)".to_string())?;
    if parsed.schema_version != EVIDENCE_SCHEMA_VERSION {
        return Err(format!(
            "versão de esquema de evidências desconhecida ({})",
            parsed.schema_version
        ));
    }
    if hex_decode_32(&parsed.salt).is_none() {
        return Err("arquivo de evidências corrompido (salt inválido)".to_string());
    }
    Ok(Some(parsed))
}

fn split_parent_and_name(path: &Path) -> Result<(&Path, &str)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("nome de arquivo de evidências inválido")?;
    Ok((parent, file_name))
}

fn ensure_parent_dir_nofollow(parent: &Path) -> Result<OwnedFd> {
    if let Ok(meta) = fs::symlink_metadata(parent) {
        if meta.file_type().is_symlink() {
            bail!(
                "diretório '{}' é um link simbólico e foi recusado",
                parent.display()
            );
        }
        if !meta.file_type().is_dir() {
            bail!("'{}' não é um diretório", parent.display());
        }
    } else {
        fs::create_dir_all(parent)
            .with_context(|| format!("falha ao criar diretório {}", parent.display()))?;
    }
    let raw = parent
        .to_str()
        .context("caminho do diretório de evidências não é UTF-8")?;
    let c_path = CString::new(raw)?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao abrir {}", parent.display()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

struct DirLockGuard {
    _file: File,
}

impl DirLockGuard {
    fn lock_at(dir_fd: BorrowedFd<'_>, lock_name: &str, wait: Duration) -> Result<Self> {
        let c_name = CString::new(lock_name)?;
        let fd = unsafe {
            libc::openat(
                dir_fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("falha ao abrir trava '{lock_name}'"));
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        unsafe {
            libc::fchmod(owned.as_raw_fd(), 0o600);
        }
        // Bounded: a stopped process holding the lock must never hang a loop or its Ctrl-C.
        let deadline = Instant::now() + wait;
        loop {
            let rc = unsafe { libc::flock(owned.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {}
                _ => return Err(err).context("falha ao adquirir trava do arquivo de evidências"),
            }
            if Instant::now() >= deadline {
                bail!("arquivo de evidências ocupado por outro processo");
            }
            std::thread::sleep(LOCK_RETRY);
        }
        Ok(Self {
            _file: File::from(owned),
        })
    }
}

/// Not synced to disk, so the lock is held briefly: this is a cache, and a file damaged by a crash
/// only means a real call.
fn write_atomic_at(dir_fd: BorrowedFd<'_>, file_name: &str, bytes: &[u8]) -> Result<()> {
    let seq = NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(".{file_name}.tmp.{}.{seq}", std::process::id());
    let c_tmp = CString::new(tmp_name.as_str())?;
    let c_dst = CString::new(file_name)?;
    let fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            c_tmp.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao criar arquivo temporário '{tmp_name}'"));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    unsafe {
        libc::fchmod(owned.as_raw_fd(), 0o600);
    }
    let mut file = File::from(owned);
    if let Err(err) = file.write_all(bytes) {
        unsafe {
            libc::unlinkat(dir_fd.as_raw_fd(), c_tmp.as_ptr(), 0);
        }
        return Err(err).context("falha ao gravar arquivo de evidências");
    }
    drop(file);
    if unsafe {
        libc::renameat(
            dir_fd.as_raw_fd(),
            c_tmp.as_ptr(),
            dir_fd.as_raw_fd(),
            c_dst.as_ptr(),
        )
    } < 0
    {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::unlinkat(dir_fd.as_raw_fd(), c_tmp.as_ptr(), 0);
        }
        return Err(err).with_context(|| format!("falha ao substituir '{file_name}'"));
    }
    Ok(())
}

impl EvidenceStore {
    pub fn open(path: &Path) -> Self {
        let mut store = Self {
            path: path.to_path_buf(),
            salt: random_salt(),
            entries: Vec::new(),
            warning: None,
            loaded_from_disk: false,
        };
        let Ok((parent, file_name)) = split_parent_and_name(path) else {
            store.warning = Some("caminho do arquivo de evidências inválido".to_string());
            return store;
        };

        let dir_fd = match open_dir_nofollow(parent) {
            Ok(Some(fd)) => fd,
            Ok(None) => return store,
            Err(warn) => {
                store.warning = Some(warn);
                return store;
            }
        };

        match read_evidence_at(dir_fd.as_fd(), file_name) {
            Ok(Some(parsed)) => {
                if let Some(salt) = hex_decode_32(&parsed.salt) {
                    store.salt = salt;
                    store.entries = parsed.entries;
                    store.loaded_from_disk = true;
                } else {
                    store.warning = Some("arquivo de evidências com salt inválido".to_string());
                }
            }
            Ok(None) => {}
            Err(warn) => {
                store.warning = Some(warn);
            }
        }
        store
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn salt(&self) -> &[u8; 32] {
        &self.salt
    }

    pub fn set_salt_if_uninitialized(&mut self, salt: [u8; 32]) {
        if !self.loaded_from_disk {
            self.salt = salt;
        }
    }

    pub fn is_valid(
        &self,
        choice: &ModelChoice,
        identity: &CliIdentity,
        now_ms: i64,
        valid_hours: u32,
    ) -> bool {
        let ttl_ms = i64::from(valid_hours) * 3_600_000;
        self.entries.iter().any(|entry| {
            entry.provider == choice.provider
                && entry.model == choice.model
                && &entry.identity == identity
                && entry.recorded_at_ms <= now_ms
                && now_ms.saturating_sub(entry.recorded_at_ms) < ttl_ms
        })
    }

    pub fn record(
        &mut self,
        choice: &ModelChoice,
        identity: &CliIdentity,
        now_ms: i64,
    ) -> Result<()> {
        let cloned = identity.clone();
        self.record_with(choice, now_ms, move |_| Some(cloned))
    }

    pub fn record_with<F>(
        &mut self,
        choice: &ModelChoice,
        now_ms: i64,
        build_identity: F,
    ) -> Result<()>
    where
        F: FnOnce(&[u8; 32]) -> Option<CliIdentity>,
    {
        let (parent, file_name) = split_parent_and_name(&self.path)?;
        let dir_fd = ensure_parent_dir_nofollow(parent)?;
        let borrowed_dir = dir_fd.as_fd();
        let _lock = DirLockGuard::lock_at(borrowed_dir, LOCK_FILE_NAME, LOCK_WAIT)?;

        let (salt, mut entries) = match read_evidence_at(borrowed_dir, file_name) {
            Ok(Some(parsed)) => (
                hex_decode_32(&parsed.salt).unwrap_or(self.salt),
                parsed.entries,
            ),
            Ok(None) | Err(_) => (self.salt, Vec::new()),
        };

        let Some(identity) = build_identity(&salt) else {
            return Ok(());
        };
        entries.retain(|e| !(e.provider == choice.provider && e.model == choice.model));
        entries.push(EvidenceEntry {
            provider: choice.provider,
            model: choice.model.clone(),
            identity,
            recorded_at_ms: now_ms,
        });

        let doc = EvidenceFile {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            salt: hex_encode(&salt),
            entries: entries.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&doc)?;
        write_atomic_at(borrowed_dir, file_name, &bytes)?;

        self.salt = salt;
        self.entries = entries;
        self.warning = None;
        self.loaded_from_disk = true;
        Ok(())
    }

    pub fn discard(&mut self, choice: &ModelChoice) -> Result<()> {
        self.entries
            .retain(|e| !(e.provider == choice.provider && e.model == choice.model));
        let (parent, file_name) = split_parent_and_name(&self.path)?;
        let dir_fd = match open_dir_nofollow(parent) {
            Ok(Some(fd)) => fd,
            Ok(None) => return Ok(()),
            Err(msg) => bail!("{msg}"),
        };
        let borrowed_dir = dir_fd.as_fd();
        let _lock = DirLockGuard::lock_at(borrowed_dir, LOCK_FILE_NAME, LOCK_WAIT)?;

        let Ok(Some(parsed)) = read_evidence_at(borrowed_dir, file_name) else {
            return Ok(());
        };
        let salt = hex_decode_32(&parsed.salt).unwrap_or(self.salt);
        let mut entries = parsed.entries;
        let before = entries.len();
        entries.retain(|e| !(e.provider == choice.provider && e.model == choice.model));
        if entries.len() != before {
            let doc = EvidenceFile {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                salt: hex_encode(&salt),
                entries: entries.clone(),
            };
            let bytes = serde_json::to_vec_pretty(&doc)?;
            write_atomic_at(borrowed_dir, file_name, &bytes)?;
        }
        self.salt = salt;
        self.entries = entries;
        Ok(())
    }
}

pub fn compute_executable_identity(bin: &str, path_var: Option<&OsStr>) -> String {
    let candidate = if bin.contains('/') {
        let p = PathBuf::from(bin);
        p.is_file().then_some(p)
    } else {
        path_var.and_then(|pv| {
            std::env::split_paths(pv)
                .map(|dir| dir.join(bin))
                .find(|cand| cand.is_file())
        })
    };

    let Some(path) = candidate else {
        return "missing".to_string();
    };
    let resolved = fs::canonicalize(&path).unwrap_or(path);
    let Ok(meta) = fs::metadata(&resolved) else {
        return "missing".to_string();
    };
    let mut hasher = Sha256::new();
    hasher.update(resolved.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(meta.len().to_le_bytes());
    hasher.update(meta.mtime().to_le_bytes());
    hasher.update(meta.mtime_nsec().to_le_bytes());
    hex_encode(&hasher.finalize())
}

pub fn compute_account_identity(salt: &[u8; 32], input: AccountIdentityInput<'_>) -> String {
    match input {
        AccountIdentityInput::ClaudeKnown {
            org_id,
            org_name,
            email,
        } => {
            let mut msg = Vec::new();
            msg.extend_from_slice(b"claude\0");
            for field in [org_id, org_name, email] {
                match field {
                    Some(v) => {
                        msg.push(1);
                        msg.extend_from_slice(&(v.len() as u64).to_le_bytes());
                        msg.extend_from_slice(v.as_bytes());
                    }
                    None => msg.push(0),
                }
            }
            format!("claude:{}", hex_encode(&hmac_sha256(salt, &msg)))
        }
        AccountIdentityInput::OpencodeKey(Some(key)) => {
            let mut msg = Vec::with_capacity(4 + key.len());
            msg.extend_from_slice(b"key\0");
            msg.extend_from_slice(key.as_bytes());
            format!("opencode:{}", hex_encode(&hmac_sha256(salt, &msg)))
        }
        AccountIdentityInput::OpencodeKey(None) => {
            format!(
                "opencode:{}",
                hex_encode(&hmac_sha256(salt, b"opencode-nokey"))
            )
        }
        AccountIdentityInput::AgyTokenPresent(true) => "present".to_string(),
        AccountIdentityInput::AgyTokenPresent(false) => "absent".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::TempDir;

    fn sample_identity(exe: &str, acct: &str, sb: &str) -> CliIdentity {
        CliIdentity {
            executable: exe.to_string(),
            account: acct.to_string(),
            sandbox: sb.to_string(),
        }
    }

    #[test]
    fn open_missing_file_returns_empty_without_creating_files_or_warning() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("loops/model_evidence.json");
        let store = EvidenceStore::open(&path);
        assert_eq!(store.warning(), None);
        assert!(!path.exists());
        assert!(!tmp.path().join("loops").exists());
    }

    #[test]
    fn executable_identity_hashes_path_size_and_mtime_without_running_binary() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let marker = tmp.path().join("invoked.marker");
        let bin = bin_dir.join("claude");
        crate::test_support::write_executable(
            &bin,
            format!("#!/bin/sh\ntouch {}\necho 9.9.9\n", marker.display()),
        );

        let id1 = compute_executable_identity("claude", Some(bin_dir.as_os_str()));
        assert!(
            !marker.exists(),
            "executable identity must never execute the CLI"
        );
        assert_ne!(id1, "missing");

        let id1_again = compute_executable_identity("claude", Some(bin_dir.as_os_str()));
        assert_eq!(id1, id1_again);

        crate::test_support::write_executable(&bin, "#!/bin/sh\necho changed-size-content\n");
        let id2 = compute_executable_identity("claude", Some(bin_dir.as_os_str()));
        assert_ne!(id1, id2);
    }

    #[test]
    fn account_identity_is_salted_and_store_contains_no_raw_secrets() {
        let tmp = TempDir::new().unwrap();
        let p1 = tmp.path().join("s1/model_evidence.json");
        let p2 = tmp.path().join("s2/model_evidence.json");
        let mut s1 = EvidenceStore::open(&p1);
        let s2 = EvidenceStore::open(&p2);

        let email = "alice@example.org";
        let org_id = "org-12345-secret";
        let org_name = "SecretCorp";
        let oc_key = "sk-opencode-top-secret-value-42";

        let c1 = compute_account_identity(
            s1.salt(),
            AccountIdentityInput::ClaudeKnown {
                org_id: Some(org_id),
                org_name: Some(org_name),
                email: Some(email),
            },
        );
        let c2 = compute_account_identity(
            s2.salt(),
            AccountIdentityInput::ClaudeKnown {
                org_id: Some(org_id),
                org_name: Some(org_name),
                email: Some(email),
            },
        );
        assert_ne!(c1, c2, "claude account digest must be salted per store");
        assert!(!c1.contains(email));
        assert!(!c1.contains(org_id));
        assert!(!c1.contains(org_name));

        let k1 =
            compute_account_identity(s1.salt(), AccountIdentityInput::OpencodeKey(Some(oc_key)));
        let k_none = compute_account_identity(s1.salt(), AccountIdentityInput::OpencodeKey(None));
        assert_ne!(k1, k_none);
        assert!(!k1.contains(oc_key));

        let agy_yes =
            compute_account_identity(s1.salt(), AccountIdentityInput::AgyTokenPresent(true));
        let agy_no =
            compute_account_identity(s1.salt(), AccountIdentityInput::AgyTokenPresent(false));
        assert_ne!(agy_yes, agy_no);

        s1.record(
            &ModelChoice {
                provider: Provider::Claude,
                model: Some("haiku".into()),
            },
            &sample_identity("exe1", &c1, "bwrap"),
            1_000_000,
        )
        .unwrap();
        s1.record(
            &ModelChoice {
                provider: Provider::Opencode,
                model: Some("opencode-go/glm".into()),
            },
            &sample_identity("exe2", &k1, "bwrap"),
            1_000_000,
        )
        .unwrap();

        let meta = fs::metadata(&p1).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let raw = fs::read_to_string(&p1).unwrap();
        assert!(!raw.contains(email));
        assert!(!raw.contains(org_id));
        assert!(!raw.contains(org_name));
        assert!(!raw.contains(oc_key));
    }

    #[test]
    fn validity_checks_exact_limit_future_and_identity_mismatches() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("model_evidence.json");
        let mut store = EvidenceStore::open(&path);
        let choice = ModelChoice {
            provider: Provider::Claude,
            model: Some("sonnet".into()),
        };
        let id = sample_identity("exe-a", "acct-a", "bwrap");
        let recorded_at = 1_700_000_000_000i64;
        store.record(&choice, &id, recorded_at).unwrap();

        let valid_hours = 12u32;
        let ttl_ms = i64::from(valid_hours) * 3_600_000;

        assert!(store.is_valid(&choice, &id, recorded_at, valid_hours));
        assert!(store.is_valid(&choice, &id, recorded_at + ttl_ms - 1, valid_hours));
        assert!(
            !store.is_valid(&choice, &id, recorded_at + ttl_ms, valid_hours),
            "evidence exactly at valid_hours limit must be expired"
        );
        assert!(
            !store.is_valid(&choice, &id, recorded_at - 1, valid_hours),
            "future timestamp must be invalid"
        );
        assert!(!store.is_valid(
            &choice,
            &sample_identity("exe-b", "acct-a", "bwrap"),
            recorded_at + 1000,
            valid_hours
        ));
        assert!(!store.is_valid(
            &choice,
            &sample_identity("exe-a", "acct-b", "bwrap"),
            recorded_at + 1000,
            valid_hours
        ));
        assert!(!store.is_valid(
            &choice,
            &sample_identity("exe-a", "acct-a", "inherited"),
            recorded_at + 1000,
            valid_hours
        ));
    }

    #[test]
    fn corrupt_or_symlink_store_falls_back_to_empty_with_warning() {
        let tmp = TempDir::new().unwrap();
        let corrupt = tmp.path().join("corrupt/model_evidence.json");
        fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        fs::write(&corrupt, "{not valid json").unwrap();
        let s1 = EvidenceStore::open(&corrupt);
        assert!(s1.warning().is_some());
        assert!(!s1.is_valid(
            &ModelChoice {
                provider: Provider::Claude,
                model: None
            },
            &sample_identity("e", "a", "bwrap"),
            1000,
            12
        ));

        let real_target = tmp.path().join("target.json");
        fs::write(&real_target, "{}").unwrap();
        let linked = tmp.path().join("linked.json");
        symlink(&real_target, &linked).unwrap();
        let s2 = EvidenceStore::open(&linked);
        assert!(s2.warning().is_some());
    }

    #[test]
    fn concurrent_writers_lose_no_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("loops/model_evidence.json");
        let id = sample_identity("exe", "acct", "bwrap");
        // Concurrent threads recording 12 distinct models at once through flock + atomic rename.
        std::thread::scope(|scope| {
            for i in 0..12usize {
                let p = &path;
                let id_ref = &id;
                scope.spawn(move || {
                    let mut st = EvidenceStore::open(p);
                    st.record(
                        &ModelChoice {
                            provider: Provider::Opencode,
                            model: Some(format!("opencode/model-{i}")),
                        },
                        id_ref,
                        1_700_000_000_000 + i as i64,
                    )
                    .unwrap();
                });
            }
        });

        let final_store = EvidenceStore::open(&path);
        assert_eq!(final_store.warning(), None);
        for i in 0..12usize {
            assert!(
                final_store.is_valid(
                    &ModelChoice {
                        provider: Provider::Opencode,
                        model: Some(format!("opencode/model-{i}")),
                    },
                    &id,
                    1_700_000_000_500,
                    12,
                ),
                "missing concurrent entry {i}"
            );
        }
    }

    #[test]
    fn a_held_lock_is_waited_for_a_bounded_time_and_released_on_drop() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_parent_dir_nofollow(tmp.path()).unwrap();
        let held = DirLockGuard::lock_at(dir.as_fd(), LOCK_FILE_NAME, LOCK_WAIT).unwrap();
        let err =
            match DirLockGuard::lock_at(dir.as_fd(), LOCK_FILE_NAME, Duration::from_millis(50)) {
                Ok(_) => panic!("a held lock was acquired twice"),
                Err(e) => e.to_string(),
            };
        assert!(err.contains("ocupado"), "{err}");
        drop(held);
        DirLockGuard::lock_at(dir.as_fd(), LOCK_FILE_NAME, LOCK_WAIT).unwrap();
    }
}
