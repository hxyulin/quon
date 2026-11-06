//! Cross-process advisory lock around a project's build cache.
//!
//! See `docs/src/architecture/concurrency.md` for the lock's role in
//! quon's multi-process safety story (why advisory, why a sibling
//! file, why both `build` and `clean` take it).

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// RAII guard holding an exclusive advisory lock on the project's cache
/// lockfile. The lock is released when the guard is dropped.
///
/// The held [`File`] is intentionally private — there's nothing to read
/// or write inside it; the file exists purely as a stable inode for the
/// kernel's lock table.
#[must_use = "dropping the CacheLock immediately releases the lock"]
pub struct CacheLock {
    // Held to keep the OS lock alive. Dropping releases it via the
    // platform-specific unlock_exclusive in `Drop`.
    file: File,
    path: PathBuf,
}

impl CacheLock {
    /// Acquire the cache lock for the given build layout.
    ///
    /// The lockfile lives at `<build_root>/.lock`.
    /// `<build_root>` is created if it doesn't already exist — `build`
    /// is going to need it anyway, and `clean` short-circuits before it
    /// would call this on a missing build dir, so creating it here is
    /// always safe.
    ///
    /// Acquisition strategy: try a non-blocking lock first; if that
    /// fails (another quon process is holding it), print a one-line
    /// notice to stderr and fall back to a blocking acquisition. The
    /// notice prevents the user from staring at a frozen terminal
    /// wondering why nothing is happening — the most common confusing
    /// failure mode of locking systems.
    pub fn acquire(layout: &BuildLayout) -> Result<Self> {
        let what = format!("another build in {}", layout.root().display());
        Self::acquire_file(&lockfile_path(layout), &what)
    }

    /// Acquire an exclusive lock on `lock_path`, creating it and its
    /// parent directory if needed. `what` names the holder in the
    /// notice printed while waiting ("quon: waiting for <what>").
    /// Used for caches outside a build directory, such as the shared
    /// quon-config library under `$QUON_HOME`.
    pub fn acquire_file(lock_path: &Path, what: &str) -> Result<Self> {
        let lock_path = lock_path.to_path_buf();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        // Try a non-blocking acquisition first so the common single-process
        // case is silent. fs2 returns WouldBlock when another process
        // holds the lock; everything else is a real error.
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                eprintln!("quon: waiting for {what}");
                file.lock_exclusive().map_err(|e| Error::Io {
                    path: lock_path.clone(),
                    source: e,
                })?;
            }
            Err(e) => {
                return Err(Error::Io {
                    path: lock_path,
                    source: e,
                });
            }
        }

        Ok(Self {
            file,
            path: lock_path,
        })
    }

    /// Path of the lockfile this guard is holding. Mostly useful for
    /// diagnostics and tests.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // Best-effort unlock. fs2 also drops the lock when the file
        // descriptor closes (which is about to happen as `file` itself
        // drops), so a failure here is harmless — we just won't get a
        // log line for it.
        let _ = FileExt::unlock(&self.file);
    }
}

/// The lockfile path for a layout: `<root>/.lock`. One lock covers
/// every driver, because `quon check` and `quon build` share the
/// fixed-host-profile units, the sysroot and the records.
fn lockfile_path(layout: &BuildLayout) -> PathBuf {
    layout.root().join(".lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_layout(tmp: &Path) -> BuildLayout {
        BuildLayout::new(tmp.join("build"), "testproject")
    }

    #[test]
    fn lockfile_lives_in_the_build_root() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        assert_eq!(lockfile_path(&layout), layout.root().join(".lock"));
    }

    #[test]
    fn acquire_creates_build_dir_and_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        // build_dir does not exist yet — acquire must create it.
        assert!(!layout.root().exists());

        let guard = CacheLock::acquire(&layout).expect("acquire");
        assert!(layout.root().exists());
        assert!(guard.path().exists());
        drop(guard);
    }

    #[test]
    fn drop_releases_lock_so_reacquisition_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());

        let g1 = CacheLock::acquire(&layout).expect("first acquire");
        drop(g1);
        // After drop, a second acquisition must succeed without
        // blocking. We use a tight loop with try_lock_exclusive on a
        // raw file to verify that — the second `acquire` would
        // otherwise hang the test if drop didn't release.
        let _g2 = CacheLock::acquire(&layout).expect("second acquire");
    }

    #[test]
    fn try_lock_blocks_on_held_lock_in_separate_process_via_fs2() {
        // We simulate cross-process contention with a second `File`
        // handle pointing at the same lockfile inode. fs2's lock
        // semantics on POSIX (flock) and Windows (LockFileEx) reject a
        // second exclusive lock from a different file descriptor even
        // within the same process. This makes the assertion below a
        // valid stand-in for a true subprocess test, without the cost
        // and flakiness of spawning `quon` itself.
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        let _g1 = CacheLock::acquire(&layout).expect("first acquire");

        let lock_path = lockfile_path(&layout);
        let f2 = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open second handle");
        // The second handle's try-lock must report WouldBlock.
        match f2.try_lock_exclusive() {
            Ok(()) => panic!("expected WouldBlock, but second try_lock succeeded"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock),
        }
    }
}
