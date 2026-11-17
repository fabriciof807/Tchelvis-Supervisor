use super::config::SupervisorConfig;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn slot_lock_path(cfg: &SupervisorConfig) -> PathBuf {
    cfg.state_file_path.with_file_name("llm_slot.lock")
}

pub struct SlotGuard {
    file: File,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn try_acquire(path: &Path) -> Result<Option<SlotGuard>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("Failed to open lock file {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(SlotGuard { file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("Failed to lock {}", path.display()))
        }
    }
}

pub fn acquire_with_wait(path: &Path, wait: Duration, poll: Duration) -> Result<Option<SlotGuard>> {
    let start = Instant::now();
    loop {
        if let Some(guard) = try_acquire(path)? {
            return Ok(Some(guard));
        }
        let elapsed = start.elapsed();
        if elapsed >= wait {
            return Ok(None);
        }
        let remaining = wait.saturating_sub(elapsed);
        let sleep_for = if poll.is_zero() {
            remaining
        } else {
            poll.min(remaining)
        };
        std::thread::sleep(sleep_for);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn second_acquirer_sees_lock_held_until_first_drops() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llm_slot.lock");

        let g1 = try_acquire(&path)
            .unwrap()
            .expect("first acquire should succeed");
        assert!(
            try_acquire(&path).unwrap().is_none(),
            "second acquire should see lock held"
        );
        drop(g1);
        let g2 = try_acquire(&path)
            .unwrap()
            .expect("acquire after drop should succeed");
        drop(g2);
    }

    #[test]
    fn acquire_with_wait_gives_up_after_wait() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("llm_slot.lock");

        let _g1 = try_acquire(&path)
            .unwrap()
            .expect("first acquire should succeed");
        let res =
            acquire_with_wait(&path, Duration::from_millis(0), Duration::from_millis(1)).unwrap();
        assert!(res.is_none());
    }
}
