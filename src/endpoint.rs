//! The local endpoint the daemon listens on.
//!
//! `server_protocol_version` 1 section 2 fixes the locations and requires that only the
//! current operating-system user can reach them. On Unix that is a Unix domain socket inside a
//! directory restricted to the user; on Windows it is a named pipe carrying an access control
//! list for the user's security identifier.
//!
//! This module opens the listener and enforces the restriction. It speaks no protocol: framing
//! and negotiation arrive with sessions in the next increment.

use std::io;
use std::path::{Path, PathBuf};

/// The directory holding the daemon's runtime files, `~/.nostdb/run`.
///
/// # Errors
///
/// Returns an error when no home directory can be determined.
pub fn run_directory() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is unset, so the per-user endpoint has no location",
            )
        })?;
    Ok(PathBuf::from(home).join(".nostdb").join("run"))
}

/// The endpoint address for the current user.
///
/// # Errors
///
/// Returns an error when no home directory can be determined on Unix. The Windows form needs
/// none, because the pipe namespace is not under a home directory.
#[cfg(unix)]
pub fn address() -> io::Result<PathBuf> {
    Ok(run_directory()?.join("nostdb.sock"))
}

/// The lock file enforcing one daemon per user.
///
/// It lives beside the endpoint rather than inside a database, because it belongs to the user's
/// machine rather than to any one graph.
///
/// # Errors
///
/// Returns an error when no home directory can be determined.
pub fn lock_path() -> io::Result<PathBuf> {
    Ok(run_directory()?.join("daemon.lock"))
}

/// A bound listener, and the address it is reachable at.
#[cfg(unix)]
#[derive(Debug)]
pub struct Listener {
    listener: std::os::unix::net::UnixListener,
    address: PathBuf,
}

#[cfg(unix)]
impl Listener {
    /// The address clients connect to.
    #[must_use]
    pub fn address(&self) -> &Path {
        &self.address
    }

    /// The bound listener.
    #[must_use]
    pub const fn as_unix_listener(&self) -> &std::os::unix::net::UnixListener {
        &self.listener
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        // The socket file does not disappear when the process exits, and a leftover one blocks
        // the next bind. It is removed here for tidiness only: `bind` below does not trust its
        // absence, because a crash leaves one behind and section 2.1 forbids treating a
        // leftover socket as proof of anything.
        let _ = std::fs::remove_file(&self.address);
    }
}

/// Binds the daemon's endpoint, replacing a leftover socket file.
///
/// The caller MUST already hold the single-instance lock. That ordering is what makes replacing
/// the socket file safe: the lock, not the socket's absence, is what proves no other daemon is
/// running, which is section 2.1's requirement. Binding first and locking second would let two
/// processes each unlink the other's socket.
///
/// # Errors
///
/// Returns an error when the run directory cannot be created or restricted, when a leftover
/// socket cannot be removed, or when the bind itself fails.
#[cfg(unix)]
pub fn bind(address: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    if let Some(directory) = address.parent() {
        std::fs::create_dir_all(directory)?;
        crate::lock::restrict_to_current_user(directory)?;
    }

    match std::fs::remove_file(address) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(address)?;

    // Belt and braces. The directory mode already prevents another user from reaching the
    // socket, and this narrows the socket itself so a future change to the directory does not
    // silently widen access.
    let mut permissions = std::fs::metadata(address)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(address, permissions)?;

    Ok(Listener {
        listener,
        address: address.to_path_buf(),
    })
}

/// The endpoint address for the current user, in the Windows pipe namespace.
///
/// The address embeds the user's security identifier, so two users on one machine name two
/// pipes and neither can reach the other's.
///
/// # Errors
///
/// Returns an error when the current user's security identifier cannot be determined.
#[cfg(not(unix))]
pub fn address() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the Windows named-pipe endpoint is not implemented in this increment",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nostdb-server-endpoint-{}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).expect("scratch directory");
        base
    }

    #[test]
    fn a_bound_socket_is_reachable_and_restricted() {
        let address = scratch("bind").join("nostdb.sock");
        let listener = super::bind(&address).expect("bound");
        assert_eq!(listener.address(), address);

        let mode = std::fs::metadata(&address)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "another user must not be able to reach the socket"
        );

        // It really is a listening socket, not just a file.
        std::os::unix::net::UnixStream::connect(&address).expect("a client can connect");
    }

    #[test]
    fn a_leftover_socket_file_does_not_block_a_bind() {
        // This is what a crashed daemon leaves behind. Section 2.1 forbids treating it as proof
        // that a daemon is running, and the caller holds the lock by the time this runs.
        let address = scratch("leftover").join("nostdb.sock");
        {
            let _first = super::bind(&address).expect("bound once");
            // Leak the socket file by forgetting the guard that would remove it.
            std::mem::forget(_first);
        }
        assert!(
            address.exists(),
            "the leftover socket is the point of this test"
        );

        let second = super::bind(&address).expect("a leftover socket is replaced, not obeyed");
        assert_eq!(second.address(), address);
    }

    #[test]
    fn the_run_directory_is_restricted_even_when_it_already_existed_wide_open() {
        let directory = scratch("wide");
        let mut wide = std::fs::metadata(&directory)
            .expect("metadata")
            .permissions();
        wide.set_mode(0o777);
        std::fs::set_permissions(&directory, wide).expect("widened");

        let _listener = super::bind(&directory.join("nostdb.sock")).expect("bound");
        let mode = std::fs::metadata(&directory)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "an inherited wide directory must be narrowed, not trusted"
        );
    }
}
