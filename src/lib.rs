//! The NostDB per-user local daemon.
//!
//! This crate coordinates local access to databases the Engine owns. It implements no database
//! behavior: it holds the named database catalog, the endpoint, the lock that keeps one daemon
//! per operating-system user, and later the sessions that call public `nostdb-core` APIs.
//!
//! # What this increment contains
//!
//! Stage 8 increment 2: the catalog, the endpoint, and the lock. There is no protocol loop yet,
//! so nothing here accepts a connection and reads a frame. [`start`] proves the lifecycle the
//! protocol contract's section 2.1 fixes, which is what the command surface will call.
//!
//! # Boundaries this crate must not cross
//!
//! - only `nostdb-core` writes `.nostdb`, and nothing here opens one;
//! - the MVP endpoint is local. There is no TCP, UDP, or HTTP listener, and the repository
//!   verifier fails if one appears;
//! - a path-based command never needs this daemon. The catalog is for names.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod diagnostic;
pub mod endpoint;
pub mod frame;
pub mod lock;
pub mod message;

use std::io;
use std::path::PathBuf;

use diagnostic::Code;

/// What a start request did.
#[derive(Debug)]
pub enum Started {
    /// This process is now the daemon.
    Running {
        /// The address clients connect to.
        address: PathBuf,
        /// The bound listener, held for the daemon's lifetime.
        listener: endpoint::Listener,
        /// The single-instance lock, held for the daemon's lifetime.
        guard: lock::Guard,
    },
    /// A healthy daemon was already running.
    ///
    /// The protocol contract's section 2.1 makes this a success that reports the existing
    /// endpoint, because starting something already started is what the caller wanted. The code
    /// is carried so a machine-readable caller can tell the two outcomes apart.
    AlreadyRunning {
        /// The address the running daemon is reachable at.
        address: PathBuf,
        /// Always [`Code::ServerAlreadyRunning`].
        code: Code,
    },
}

/// Acquires the single-instance lock and binds the endpoint.
///
/// The lock is taken **before** the endpoint is bound. That ordering is the contract: the lock,
/// not the presence or absence of a socket file, is what decides whether a daemon is running.
/// Binding first would let two processes each replace the other's socket, and probing the socket
/// instead of locking is what section 2.1 rules out.
///
/// # Errors
///
/// Returns an error when the runtime directory cannot be created or restricted, when the lock
/// file cannot be opened, or when the endpoint cannot be bound. Finding another daemon is not an
/// error; it is [`Started::AlreadyRunning`].
pub fn start() -> io::Result<Started> {
    let address = endpoint::address()?;
    let lock_path = endpoint::lock_path()?;

    match lock::acquire(&lock_path)? {
        lock::Outcome::AlreadyHeld => Ok(Started::AlreadyRunning {
            address,
            code: Code::ServerAlreadyRunning,
        }),
        lock::Outcome::Acquired(guard) => {
            let listener = endpoint::bind(&address)?;
            Ok(Started::Running {
                address,
                listener,
                guard,
            })
        }
    }
}

/// Whether a daemon is currently running for this user.
///
/// This asks the lock, not the endpoint. A leftover socket file from a crashed daemon would
/// answer this question wrongly, which is why section 2.1 forbids using it.
///
/// # Errors
///
/// Returns an error when the lock file cannot be opened.
pub fn is_running() -> io::Result<bool> {
    let lock_path = endpoint::lock_path()?;
    match lock::acquire(&lock_path)? {
        // Acquiring succeeded, so nothing held it. Releasing immediately is correct: this is a
        // question, not a claim.
        lock::Outcome::Acquired(_) => Ok(false),
        lock::Outcome::AlreadyHeld => Ok(true),
    }
}
