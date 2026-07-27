//! Sessions, as `server_protocol_version` 1 section 6 defines them.
//!
//! A session is the unit of isolation, and a connection carries at most one. Section 6.1 states
//! that, and this module is where it is enforced: [`Slot`] holds either no session or one, and
//! there is no representation for two.
//!
//! # Why a connection carries one
//!
//! The Engine's `Transaction<'a>` borrows `&'a mut Database`, so a transaction cannot outlive the
//! scope that owns the database. The daemon therefore runs a transaction as a lexical region, the
//! way `nostdb-cli`'s REPL does, and a connection inside that region is committed to one session's
//! view. Multiplexing would mean queueing another session's requests, which is buffering a client
//! cannot see, or refusing them, which is a failure it cannot predict.
//!
//! Concurrency comes from more connections, not from more sessions per connection.

use std::path::{Path, PathBuf};

use nostdb_core::storage::{Database, StorageError};

use crate::catalog::Catalog;
use crate::message::{Refusal, Rule};

/// A session's identifier.
///
/// Opaque to the client, and minted by the daemon rather than accepted from one: a client that
/// chose its own could name a session belonging to another connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// The identifier as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a session could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// The message broke a protocol rule.
    Refused(Refusal),
    /// The catalog holds no such name.
    ///
    /// Distinct from a stale target: the name is simply not registered, and the fix is
    /// `catalog add` rather than mounting a disk.
    UnknownName(String),
    /// The catalog could not be read.
    Catalog(crate::catalog::Error),
    /// The database itself could not be opened.
    ///
    /// This is the stale-target case the catalog contract's section 1.3 keeps out of catalog
    /// validation: the entry is legitimate and the target is not reachable right now.
    Storage(StorageError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => write!(formatter, "{refusal}"),
            Self::UnknownName(name) => {
                write!(formatter, "the catalog holds no database named {name}")
            }
            Self::Catalog(error) => write!(formatter, "{error}"),
            Self::Storage(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// An open session: one database, and the name it was reached by.
#[derive(Debug)]
pub struct Session {
    id: SessionId,
    name: String,
    path: PathBuf,
    database: Database,
}

impl Session {
    /// The session's identifier.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.id
    }

    /// The catalog name this session was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The database's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The database, for the Engine to run a transaction over.
    ///
    /// Exposed as a mutable borrow rather than moved out, because a transaction borrows it and
    /// must not outlive the session that owns it.
    pub const fn database_mut(&mut self) -> &mut Database {
        &mut self.database
    }
}

/// A connection's session, of which there is at most one.
///
/// This type is the enforcement of section 6.1. There is no variant holding two, so a second
/// `open_session` cannot be honoured by construction rather than by a check somebody could
/// forget.
#[derive(Debug, Default)]
pub struct Slot {
    session: Option<Session>,
    next: u64,
}

impl Slot {
    /// A connection with no session yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            session: None,
            next: 1,
        }
    }

    /// Opens the connection's session.
    ///
    /// # Errors
    ///
    /// Returns [`Rule::SecondSessionOnOneConnection`] when the connection already has one, and
    /// [`OpenError::UnknownName`] when the catalog does not hold the name. A stale target is
    /// [`OpenError::Storage`]: the catalog entry is valid and the database is not reachable, which
    /// the catalog contract's section 1.3 deliberately keeps apart.
    pub fn open(&mut self, name: &str, catalog: &Catalog) -> Result<SessionId, OpenError> {
        if let Some(existing) = &self.session {
            return Err(OpenError::Refused(Refusal::of(
                Rule::SecondSessionOnOneConnection,
                format!(
                    "this connection already holds session {}; open another connection instead",
                    existing.id
                ),
            )));
        }

        let entry = catalog
            .get(name)
            .ok_or_else(|| OpenError::UnknownName(name.to_owned()))?;
        let path = entry.path().to_path_buf();
        let database = Database::open(&path).map_err(OpenError::Storage)?;

        // The identifier is minted here and is per connection, so it need only be unique within
        // one. A client cannot name a session on another connection because it never sees one.
        let id = SessionId(format!("s{}", self.next));
        self.next += 1;

        self.session = Some(Session {
            id: id.clone(),
            name: name.to_owned(),
            path,
            database,
        });
        // Returned by value rather than as a borrow of the slot. A borrow would have to be
        // recovered from the Option just inserted, which means an unwrap that cannot fail and
        // still has to be explained.
        Ok(id)
    }

    /// The session a request names, checking it against the one this connection holds.
    ///
    /// A request naming no session gets the connection's, which is what makes `session_id`
    /// confirmation rather than routing. A request naming a different one is refused: the
    /// isolation it believes it has does not exist here.
    ///
    /// # Errors
    ///
    /// Returns [`Rule::UnknownSession`] when the connection has no session, or has one the
    /// request did not name.
    pub fn resolve(&mut self, named: Option<&str>) -> Result<&mut Session, Refusal> {
        let Some(session) = &mut self.session else {
            return Err(Refusal::of(
                Rule::UnknownSession,
                "this connection has no open session; send open_session first",
            ));
        };
        if let Some(named) = named.filter(|named| *named != session.id.as_str()) {
            return Err(Refusal::of(
                Rule::UnknownSession,
                format!(
                    "this connection holds session {}, and the request names {named}",
                    session.id
                ),
            ));
        }
        Ok(session)
    }

    /// Ends the connection's session, reporting whether there was one.
    ///
    /// Dropping the [`Session`] drops its [`Database`]. Any transaction was a lexical region
    /// inside a request, so there is none open here to roll back: section 6.2's requirement is
    /// met by the region ending, not by cleanup at close.
    pub fn close(&mut self) -> bool {
        self.session.take().is_some()
    }

    /// Whether this connection currently holds a session.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.session.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;
    use crate::catalog::Catalog;
    use crate::message::Rule;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nostdb-server-session-{}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).expect("scratch directory");
        base
    }

    /// A catalog naming one real, empty database.
    fn catalog_with_a_database(label: &str) -> (Catalog, PathBuf) {
        let path = scratch(label).join("root.nostdb");
        let _ = std::fs::remove_file(&path);
        nostdb_core::storage::Database::create(&path).expect("created");

        let mut catalog = Catalog::default();
        catalog.insert("work", &path).expect("registered");
        (catalog, path)
    }

    #[test]
    fn a_session_opens_against_a_catalogued_database() {
        let (catalog, path) = catalog_with_a_database("open");
        let mut slot = Slot::new();
        let id = slot.open("work", &catalog).expect("opened");
        assert_eq!(id.as_str(), "s1");
        assert!(slot.is_open());
        assert_eq!(slot.resolve(None).expect("resolved").path(), path);
    }

    #[test]
    fn a_second_open_session_on_one_connection_is_refused() {
        // Section 6.1, enforced by Slot holding at most one session rather than by a check.
        let (catalog, _) = catalog_with_a_database("second");
        let mut slot = Slot::new();
        slot.open("work", &catalog).expect("first");
        match slot.open("work", &catalog) {
            Err(super::OpenError::Refused(refusal)) => {
                assert_eq!(refusal.rule(), Rule::SecondSessionOnOneConnection);
            }
            other => panic!("a connection carries one session, got {other:?}"),
        }
    }

    #[test]
    fn a_name_the_catalog_does_not_hold_is_not_a_storage_failure() {
        // These are different problems with different fixes: register the name, or mount the disk.
        let (catalog, _) = catalog_with_a_database("unknown-name");
        let mut slot = Slot::new();
        match slot.open("absent", &catalog) {
            Err(super::OpenError::UnknownName(name)) => assert_eq!(name, "absent"),
            other => panic!("expected an unknown name, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_catalog_target_fails_the_open_rather_than_the_catalog() {
        // The catalog contract's section 1.3 keeps this out of catalog validation, so it has to
        // surface here instead.
        let mut catalog = Catalog::default();
        catalog
            .insert("work", Path::new("/nonexistent/disk/root.nostdb"))
            .expect("a stale target is a valid entry");
        let mut slot = Slot::new();
        assert!(matches!(
            slot.open("work", &catalog),
            Err(super::OpenError::Storage(_))
        ));
        assert!(!slot.is_open(), "a failed open must leave no session");
    }

    #[test]
    fn a_request_naming_another_session_is_refused() {
        let (catalog, _) = catalog_with_a_database("wrong-id");
        let mut slot = Slot::new();
        slot.open("work", &catalog).expect("opened");
        let refusal = slot.resolve(Some("s99")).expect_err("refused");
        assert_eq!(refusal.rule(), Rule::UnknownSession);
    }

    #[test]
    fn a_request_before_any_session_is_refused_as_unknown_session() {
        let mut slot = Slot::new();
        let refusal = slot.resolve(None).expect_err("refused");
        assert_eq!(refusal.rule(), Rule::UnknownSession);
    }

    #[test]
    fn closing_reports_whether_there_was_a_session() {
        let (catalog, _) = catalog_with_a_database("close");
        let mut slot = Slot::new();
        assert!(!slot.close(), "there was nothing to close");
        slot.open("work", &catalog).expect("opened");
        assert!(slot.close());
        assert!(!slot.is_open());
    }

    #[test]
    fn a_reopened_session_gets_a_new_identifier() {
        // A closed session's identifier is not reused, so a stale request cannot land in a new
        // session that happens to have the same name.
        let (catalog, _) = catalog_with_a_database("reopen");
        let mut slot = Slot::new();
        assert_eq!(slot.open("work", &catalog).expect("first").as_str(), "s1");
        slot.close();
        assert_eq!(slot.open("work", &catalog).expect("second").as_str(), "s2");
    }
}
