use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

pub const EXCHANGE_DIR: &str = ".tchelvis-loop";
pub const IN_DIR: &str = "in";
pub const OUT_DIR: &str = "out";
pub const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;

fn validate_artifact_filename(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        bail!("nome de artefato inválido: {name:?}");
    }
    Ok(())
}

fn open_root_dir(path: &Path) -> Result<OwnedFd> {
    let raw = path
        .to_str()
        .with_context(|| format!("caminho não é UTF-8 válido: {}", path.display()))?;
    let c_path = CString::new(raw).context("caminho contém byte nulo")?;
    // SAFETY: c_path is a valid NUL-terminated string; returns a new fd or -1.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        bail!("falha ao abrir diretório {}: {err}", path.display());
    }
    // SAFETY: fd is a valid open file descriptor owned by us.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_dir_nofollow(
    parent: BorrowedFd<'_>,
    name: &str,
) -> std::result::Result<OwnedFd, std::io::Error> {
    let c_name = CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL byte in name"))?;
    // SAFETY: parent is an open directory fd and c_name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a newly opened descriptor owned by us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn map_open_dir_error(name: &str, err: std::io::Error) -> anyhow::Error {
    match err.raw_os_error() {
        Some(libc::ELOOP) => {
            anyhow::anyhow!("caminho '{name}' é um link simbólico e foi recusado")
        }
        Some(libc::ENOTDIR) => anyhow::anyhow!("caminho '{name}' não é um diretório"),
        _ => anyhow::anyhow!("falha ao abrir diretório '{name}': {err}"),
    }
}

fn ensure_dir_nofollow(parent: BorrowedFd<'_>, name: &str) -> Result<OwnedFd> {
    match open_dir_nofollow(parent, name) {
        Ok(fd) => Ok(fd),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let c_name = CString::new(name)?;
            // SAFETY: parent is an open directory fd and c_name is NUL-terminated.
            let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o755) };
            if rc < 0 {
                let mk_err = std::io::Error::last_os_error();
                if mk_err.kind() != std::io::ErrorKind::AlreadyExists {
                    bail!("falha ao criar diretório '{name}': {mk_err}");
                }
            }
            open_dir_nofollow(parent, name).map_err(|e| map_open_dir_error(name, e))
        }
        Err(err) => Err(map_open_dir_error(name, err)),
    }
}

struct DirHandle(*mut libc::DIR);

impl Drop for DirHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 was returned by fdopendir and has not been closed yet.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
}

fn clear_dir_nofollow(dir: BorrowedFd<'_>) -> Result<()> {
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) duplicates an open directory fd.
    let dup_fd = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return Err(std::io::Error::last_os_error()).context("falha ao duplicar fd de diretório");
    }
    // SAFETY: fdopendir takes ownership of dup_fd on success.
    let dirp = unsafe { libc::fdopendir(dup_fd) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup_fd) };
        return Err(err).context("falha em fdopendir");
    }
    let handle = DirHandle(dirp);

    let mut entries: Vec<CString> = Vec::new();
    loop {
        // SAFETY: the stream is open; a read error is treated as the end of the directory and the
        // removal of its parent then fails.
        let ent = unsafe { libc::readdir(handle.0) };
        if ent.is_null() {
            break;
        }
        // SAFETY: ent is non-null and points to a valid dirent with NUL-terminated d_name.
        let name_cstr = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = name_cstr.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        entries.push(name_cstr.to_owned());
    }
    drop(handle);

    for entry in entries {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: dir is a valid open directory fd; AT_SYMLINK_NOFOLLOW inspects the entry itself.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                entry.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            bail!(
                "falha ao inspecionar entrada '{}': {err}",
                entry.to_string_lossy()
            );
        }

        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            let child_name = entry.to_string_lossy();
            let child_fd = open_dir_nofollow(dir, &child_name)
                .map_err(|e| map_open_dir_error(&child_name, e))?;
            clear_dir_nofollow(child_fd.as_fd())?;
            drop(child_fd);
            // SAFETY: entry is a directory inside dir.
            let urc =
                unsafe { libc::unlinkat(dir.as_raw_fd(), entry.as_ptr(), libc::AT_REMOVEDIR) };
            if urc < 0 {
                let err = std::io::Error::last_os_error();
                bail!("falha ao remover diretório '{}': {err}", child_name);
            }
        } else {
            // Regular file, symlink, FIFO, etc.: unlink without following symlinks.
            // SAFETY: entry is a non-directory entry inside dir.
            let urc = unsafe { libc::unlinkat(dir.as_raw_fd(), entry.as_ptr(), 0) };
            if urc < 0 {
                let err = std::io::Error::last_os_error();
                bail!(
                    "falha ao remover entrada '{}': {err}",
                    entry.to_string_lossy()
                );
            }
        }
    }
    Ok(())
}

pub fn prepare_exchange(workspace: &Path, reads: &[(String, PathBuf)]) -> Result<()> {
    let mut loaded: Vec<(String, Vec<u8>)> = Vec::with_capacity(reads.len());
    for (name, src) in reads {
        validate_artifact_filename(name)?;
        let bytes = fs::read(src)
            .with_context(|| format!("falha ao ler artefato de entrada '{}'", src.display()))?;
        loaded.push((name.clone(), bytes));
    }

    let ws_fd = open_root_dir(workspace)?;
    let ex_fd = ensure_dir_nofollow(ws_fd.as_fd(), EXCHANGE_DIR)?;
    let in_fd = ensure_dir_nofollow(ex_fd.as_fd(), IN_DIR)?;
    let out_fd = ensure_dir_nofollow(ex_fd.as_fd(), OUT_DIR)?;

    clear_dir_nofollow(in_fd.as_fd())?;
    clear_dir_nofollow(out_fd.as_fd())?;

    for (name, bytes) in loaded {
        let c_name = CString::new(name.as_str())?;
        // SAFETY: in_fd is a verified non-symlink directory; O_CREAT|O_EXCL|O_NOFOLLOW cannot follow symlinks.
        let fd = unsafe {
            libc::openat(
                in_fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            bail!("falha ao criar artefato de entrada '{name}': {err}");
        }
        // SAFETY: fd is a newly opened writable file descriptor owned by us.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)
            .with_context(|| format!("falha ao gravar artefato de entrada '{name}'"))?;
        file.flush()?;
    }

    Ok(())
}

pub fn collect_output(workspace: &Path, name: &str) -> Result<String> {
    validate_artifact_filename(name)?;

    let ws_fd = open_root_dir(workspace)?;
    let ex_fd = open_dir_nofollow(ws_fd.as_fd(), EXCHANGE_DIR)
        .map_err(|e| map_open_dir_error(EXCHANGE_DIR, e))?;
    let out_fd =
        open_dir_nofollow(ex_fd.as_fd(), OUT_DIR).map_err(|e| map_open_dir_error(OUT_DIR, e))?;

    let c_name = CString::new(name)?;
    // SAFETY: out_fd is a verified non-symlink directory; O_NOFOLLOW blocks symlink files,
    // O_NONBLOCK prevents blocking when opening a FIFO before fstat rejects it.
    let fd = unsafe {
        libc::openat(
            out_fd.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            bail!("artefato '{name}' é um link simbólico e foi recusado");
        }
        bail!("falha ao abrir artefato '{name}': {err}");
    }
    // SAFETY: fd is a newly opened descriptor owned by us.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: owned_fd is a valid open file descriptor.
    if unsafe { libc::fstat(owned_fd.as_raw_fd(), &mut st) } < 0 {
        let err = std::io::Error::last_os_error();
        bail!("falha em fstat para '{name}': {err}");
    }

    let file_type = st.st_mode & libc::S_IFMT;
    if file_type != libc::S_IFREG {
        bail!("artefato '{name}' não é um arquivo regular");
    }
    if st.st_size < 0 || st.st_size as usize > MAX_ARTIFACT_BYTES {
        bail!(
            "artefato '{name}' excede o limite de {} bytes ({} bytes)",
            MAX_ARTIFACT_BYTES,
            st.st_size
        );
    }

    let file = File::from(owned_fd);
    let mut buf = Vec::with_capacity((st.st_size as usize).min(MAX_ARTIFACT_BYTES));
    file.take((MAX_ARTIFACT_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .with_context(|| format!("falha ao ler artefato '{name}'"))?;
    if buf.len() > MAX_ARTIFACT_BYTES {
        bail!("artefato '{name}' excede o limite de {MAX_ARTIFACT_BYTES} bytes");
    }

    String::from_utf8(buf).with_context(|| format!("artefato '{name}' não está em UTF-8 válido"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn mkfifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    }

    #[test]
    fn symlinked_output_parent_or_exchange_dir_is_refused() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        fs::write(&secret, "top-secret").unwrap();

        // 1) output file itself is a symlink
        {
            let ws = tmp.path().join("ws_file_link");
            let out_dir = ws.join(EXCHANGE_DIR).join(OUT_DIR);
            fs::create_dir_all(&out_dir).unwrap();
            symlink(&secret, out_dir.join("review.md")).unwrap();
            let err = collect_output(&ws, "review.md").unwrap_err();
            assert!(!err.to_string().is_empty());
        }

        // 2) `out` directory is a symlink
        {
            let ws = tmp.path().join("ws_out_link");
            let exchange = ws.join(EXCHANGE_DIR);
            fs::create_dir_all(&exchange).unwrap();
            symlink(&outside, exchange.join(OUT_DIR)).unwrap();
            let err = collect_output(&ws, "secret.txt").unwrap_err();
            assert!(!err.to_string().is_empty());
        }

        // 3) `.tchelvis-loop` directory is a symlink
        {
            let ws = tmp.path().join("ws_exchange_link");
            fs::create_dir_all(&ws).unwrap();
            let outside_exchange = tmp.path().join("outside_exchange");
            fs::create_dir_all(outside_exchange.join(OUT_DIR)).unwrap();
            fs::write(outside_exchange.join(OUT_DIR).join("review.md"), "leaked").unwrap();
            symlink(&outside_exchange, ws.join(EXCHANGE_DIR)).unwrap();
            let err = collect_output(&ws, "review.md").unwrap_err();
            assert!(!err.to_string().is_empty());
        }

        // 4) FIFO, directory, 1 MiB + 1, invalid UTF-8 refused
        {
            let ws = tmp.path().join("ws_types");
            let out_dir = ws.join(EXCHANGE_DIR).join(OUT_DIR);
            fs::create_dir_all(&out_dir).unwrap();

            // FIFO must fail immediately without hanging
            mkfifo(&out_dir.join("fifo.md"));
            assert!(collect_output(&ws, "fifo.md").is_err());

            // Directory
            fs::create_dir_all(out_dir.join("dir.md")).unwrap();
            assert!(collect_output(&ws, "dir.md").is_err());

            // 1 MiB + 1
            let too_big = vec![b'a'; MAX_ARTIFACT_BYTES + 1];
            fs::write(out_dir.join("big.md"), &too_big).unwrap();
            assert!(collect_output(&ws, "big.md").is_err());

            // Exactly 1 MiB is allowed
            let exact = vec![b'a'; MAX_ARTIFACT_BYTES];
            fs::write(out_dir.join("exact.md"), &exact).unwrap();
            assert_eq!(
                collect_output(&ws, "exact.md").unwrap().len(),
                MAX_ARTIFACT_BYTES
            );

            // Invalid UTF-8
            fs::write(out_dir.join("bad_utf8.md"), [0xff, 0xfe, 0xfd]).unwrap();
            assert!(collect_output(&ws, "bad_utf8.md").is_err());
        }

        // 5) prepare_exchange never writes through a symlink
        {
            let src_file = tmp.path().join("input_src.md");
            fs::write(&src_file, "new-content").unwrap();

            // Case A: `.tchelvis-loop/in` is a symlink to outside dir
            let ws_a = tmp.path().join("ws_prep_in_link");
            fs::create_dir_all(ws_a.join(EXCHANGE_DIR)).unwrap();
            let outside_in = tmp.path().join("outside_in");
            fs::create_dir_all(&outside_in).unwrap();
            symlink(&outside_in, ws_a.join(EXCHANGE_DIR).join(IN_DIR)).unwrap();
            let res_a = prepare_exchange(&ws_a, &[("input.md".into(), src_file.clone())]);
            assert!(res_a.is_err());
            assert!(!outside_in.join("input.md").exists());

            // Case B: `.tchelvis-loop/in/input.md` is a symlink to outside file
            let ws_b = tmp.path().join("ws_prep_file_link");
            fs::create_dir_all(ws_b.join(EXCHANGE_DIR).join(IN_DIR)).unwrap();
            let protected = tmp.path().join("protected.txt");
            fs::write(&protected, "original").unwrap();
            symlink(
                &protected,
                ws_b.join(EXCHANGE_DIR).join(IN_DIR).join("input.md"),
            )
            .unwrap();
            let _ = prepare_exchange(&ws_b, &[("input.md".into(), src_file.clone())]);
            assert_eq!(fs::read_to_string(&protected).unwrap(), "original");

            // Case C: `.tchelvis-loop` itself is a symlink
            let ws_c = tmp.path().join("ws_prep_root_link");
            fs::create_dir_all(&ws_c).unwrap();
            let outside_root = tmp.path().join("outside_root");
            fs::create_dir_all(&outside_root).unwrap();
            symlink(&outside_root, ws_c.join(EXCHANGE_DIR)).unwrap();
            let res_c = prepare_exchange(&ws_c, &[("input.md".into(), src_file)]);
            assert!(res_c.is_err());
            assert!(!outside_root.join(IN_DIR).exists());
        }
    }
}
