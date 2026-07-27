//! The lock that enforces at most one daemon per operating-system user.
//!
//! The protocol contract's section 2.1 requires an operating-system lock rather than a check
//! of whether the endpoint answers, and it requires a stale lock to be reclaimed. Both fall
//! out of one property of an advisory file lock: the operating system releases it when the
//! holding process dies, however it died.
//!
//! That is why nothing here reads a process id or probes the socket. A leftover lock file
//! whose owner is gone is not locked, so acquiring it simply succeeds. Recording a process id
//! and asking whether it is alive would reintroduce exactly the guess the contract rules out,
//! and it would be wrong whenever the id had been reused.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// A held single-instance lock.
///
/// Dropping this releases the lock. The lock file is left in place deliberately: removing it
/// would race with another process that has already opened it and is waiting to lock it.
#[derive(Debug)]
pub struct Guard {
    _file: File,
    path: PathBuf,
}

impl Guard {
    /// The lock file this guard holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The outcome of trying to become the one daemon for this user.
#[derive(Debug)]
pub enum Outcome {
    /// The lock was acquired, and this process is the daemon.
    Acquired(Guard),
    /// Another live process holds the lock.
    ///
    /// This is what `SERVER_ALREADY_RUNNING` reports, and it is not a failure: a start request
    /// that finds a healthy daemon succeeded at what the caller wanted.
    AlreadyHeld,
}

/// Tries to acquire the single-instance lock at `path`.
///
/// # Errors
///
/// Returns an error when the lock file's directory cannot be created, or the file cannot be
/// opened. Contention is not an error: it is [`Outcome::AlreadyHeld`], because the caller acts
/// on it rather than failing.
pub fn acquire(path: &Path) -> io::Result<Outcome> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
        restrict_to_current_user(directory)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    // `WouldBlock` is contention, which is an outcome rather than a failure: it is exactly the
    // "a daemon is already running" answer the caller acts on. Every other error is a real one.
    match file.try_lock() {
        Ok(()) => Ok(Outcome::Acquired(Guard {
            _file: file,
            path: path.to_path_buf(),
        })),
        Err(fs::TryLockError::WouldBlock) => Ok(Outcome::AlreadyHeld),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Restricts a directory to the current user.
///
/// On Unix this is mode 0700. The protocol contract's section 2 requires the directory holding
/// the endpoint not to be world-writable, and requires the restriction to be verified rather
/// than assumed: a directory inherited from an earlier run may have been created with a wider
/// mode, and this resets it rather than trusting it.
///
/// # Errors
///
/// Returns an error when the directory's metadata cannot be read or its mode cannot be set. A
/// failure here is not recoverable by continuing: the contract forbids widening the endpoint's
/// permissions to work around a local access failure, so the caller must stop rather than bind
/// something another user could reach.
#[cfg(unix)]
pub fn restrict_to_current_user(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(directory)?.permissions();
    if permissions.mode() & 0o777 != 0o700 {
        permissions.set_mode(0o700);
        fs::set_permissions(directory, permissions)?;
    }
    Ok(())
}

/// Restricts a directory to the current user.
///
/// On Windows the equivalent is an access control list on the named pipe, which the endpoint
/// carries rather than the directory. It is applied where the pipe is created, so there is
/// nothing to do here.
///
/// # Errors
///
/// Never returns an error on this platform. The signature matches the Unix one so callers need
/// no platform branch of their own.
#[cfg(not(unix))]
pub fn restrict_to_current_user(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Outcome, acquire};
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("nostdb-server-lock-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&base).expect("scratch directory");
        base
    }

    #[test]
    fn a_first_acquisition_succeeds() {
        let path = scratch("first").join("daemon.lock");
        let _ = std::fs::remove_file(&path);
        match acquire(&path).expect("no io failure") {
            Outcome::Acquired(guard) => assert_eq!(guard.path(), path),
            Outcome::AlreadyHeld => panic!("nothing else holds this lock"),
        }
    }

    #[test]
    fn a_second_acquisition_while_held_reports_already_held() {
        let path = scratch("second").join("daemon.lock");
        let _ = std::fs::remove_file(&path);
        let first = acquire(&path).expect("no io failure");
        assert!(matches!(first, Outcome::Acquired(_)));

        match acquire(&path).expect("no io failure") {
            Outcome::AlreadyHeld => {}
            Outcome::Acquired(_) => panic!("two processes cannot both be the daemon"),
        }
    }

    #[test]
    fn a_released_lock_can_be_acquired_again() {
        // This is the stale-lock reclaim the contract requires, in the form the operating
        // system provides it: the lock file survives, the lock does not.
        let path = scratch("released").join("daemon.lock");
        let _ = std::fs::remove_file(&path);

        let first = acquire(&path).expect("no io failure");
        assert!(matches!(first, Outcome::Acquired(_)));
        drop(first);

        assert!(path.exists(), "the lock file is left in place on purpose");
        match acquire(&path).expect("no io failure") {
            Outcome::Acquired(_) => {}
            Outcome::AlreadyHeld => {
                panic!("a lock whose holder is gone is not held, so this must succeed")
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_directory_is_restricted_to_the_current_user() {
        use std::os::unix::fs::PermissionsExt;

        let directory = scratch("permissions");
        // Widen it first, so the check proves the code narrows it rather than that the
        // temporary directory happened to be narrow already.
        let mut wide = std::fs::metadata(&directory)
            .expect("metadata")
            .permissions();
        wide.set_mode(0o777);
        std::fs::set_permissions(&directory, wide).expect("widened");

        let path = directory.join("daemon.lock");
        let _ = std::fs::remove_file(&path);
        let _guard = acquire(&path).expect("no io failure");

        let mode = std::fs::metadata(&directory)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "the endpoint directory must not be reachable by another user"
        );
    }
}
