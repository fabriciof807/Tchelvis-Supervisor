use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Writes an executable file without the current test process ever holding a
/// write file descriptor to `path`.
///
/// In parallel `cargo test` runs, another thread calling `fork()`/`exec()` can
/// briefly inherit open write fds from this process across `fork()`, causing
/// subsequent `exec()` calls on the written script to fail with `ETXTBSY`
/// (`Text file busy`). Writing to a sibling temp file, closing its handle, and
/// having a short-lived `install -m 755` child process create `path` ensures
/// this process never opens `path` for writing.
pub fn write_executable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let dst = path.as_ref();
    let parent = match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".write-exec-")
        .tempfile_in(parent)
        .unwrap_or_else(|e| {
            panic!(
                "failed creating sibling temp file in {}: {e}",
                parent.display()
            )
        });
    tmp.write_all(contents.as_ref())
        .unwrap_or_else(|e| panic!("failed writing temp script for {}: {e}", dst.display()));
    tmp.flush()
        .unwrap_or_else(|e| panic!("failed flushing temp script for {}: {e}", dst.display()));
    let src = tmp.into_temp_path();
    let status = Command::new("install")
        .arg("-m")
        .arg("755")
        .arg(&src)
        .arg(dst)
        .status()
        .unwrap_or_else(|e| panic!("failed spawning install for {}: {e}", dst.display()));
    assert!(
        status.success(),
        "install -m 755 {} {} failed with {status}",
        src.display(),
        dst.display()
    );
    src.close().unwrap_or_else(|e| {
        panic!(
            "failed removing sibling temp file for {}: {e}",
            dst.display()
        )
    });
}
