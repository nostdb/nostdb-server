//! The named database catalog, as `catalog_version` 1 defines it.
//!
//! The contract is `docs/CATALOG.md` in `nostdb-spec`, and `tests/catalog_conformance.rs` runs
//! this module against the fixtures published there.
//!
//! Three rules in that contract shape this module more than the others:
//!
//! - a stale entry target is not a malformed catalog, so nothing here touches the filesystem
//!   to validate an entry;
//! - an unknown member is preserved on rewrite, so the document is held as a JSON value rather
//!   than deserialized into a struct that would drop what it does not know;
//! - a write is a complete replacement moved into place, so a reader sees one catalog or the
//!   other and never a prefix of both.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::diagnostic::Code;

/// The versions of the catalog contract this build reads.
pub const SUPPORTED_VERSIONS: &[u64] = &[1];

/// The version this build writes.
pub const CURRENT_VERSION: u64 = 1;

/// Why a catalog was refused.
///
/// A rejection carries every problem found rather than the first, because a hand-edited
/// catalog should be fixable in one pass. That is the contract's section 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    code: Code,
    problems: Vec<String>,
}

impl Rejection {
    /// The diagnostic code a client matches on.
    #[must_use]
    pub const fn code(&self) -> Code {
        self.code
    }

    /// Every problem found, in the order they were found.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.code)?;
        for problem in &self.problems {
            write!(formatter, "\n  {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Rejection {}

/// A failure reading or writing a catalog.
#[derive(Debug)]
pub enum Error {
    /// The document was read but broke the contract.
    Rejected(Rejection),
    /// The catalog could not be read or written at all.
    ///
    /// A missing catalog is not this: [`Catalog::load`] treats absence as an empty catalog,
    /// because a user who has registered no name does not have a broken installation.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(rejection) => write!(formatter, "{rejection}"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One catalog entry: a name's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    path: PathBuf,
    /// Members this build did not write and does not interpret, kept so a rewrite preserves
    /// them. The contract's section 7 requires this.
    unknown: Map<String, Value>,
}

impl Entry {
    /// The absolute path the name refers to.
    ///
    /// This is what the catalog recorded. It is not a promise that anything exists there;
    /// the contract's section 1.3 is explicit that a stale target is not a catalog problem.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A parsed catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    databases: BTreeMap<String, Entry>,
    unknown: Map<String, Value>,
}

impl Catalog {
    /// The catalog path for the current user, `~/.nostdb/catalog.json`.
    ///
    /// # Errors
    ///
    /// Returns an error when no home directory can be determined, because a per-user catalog
    /// has no meaning without one and guessing a location would put a user's names somewhere
    /// they would never look for them.
    pub fn default_path() -> Result<PathBuf, Error> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "HOME is unset, so the per-user catalog has no location",
                )
            })?;
        Ok(PathBuf::from(home).join(".nostdb").join("catalog.json"))
    }

    /// Reads the catalog at `path`, treating absence as an empty catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Rejected`] when the document breaks the contract, and [`Error::Io`]
    /// when it exists and cannot be read. An absent file is not an error.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(Error::Rejected),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// Parses a catalog document.
    ///
    /// # Errors
    ///
    /// Returns a [`Rejection`] listing every problem found, not only the first.
    pub fn parse(text: &str) -> Result<Self, Rejection> {
        let document: Value = serde_json::from_str(text).map_err(|error| Rejection {
            code: Code::CatalogInvalid,
            problems: vec![format!("the catalog is not valid JSON: {error}")],
        })?;

        let Some(object) = document.as_object() else {
            return Err(Rejection {
                code: Code::CatalogInvalid,
                problems: vec!["a catalog is a JSON object".to_owned()],
            });
        };

        // The version is decided first and on its own. Every other rule is stated by a version,
        // so reporting a name problem in a document whose version this build cannot read would
        // be reporting against rules that may not apply to it.
        match object.get("catalog_version") {
            None => {
                return Err(Rejection {
                    code: Code::CatalogInvalid,
                    problems: vec![
                        "catalog_version is absent, and it is what makes every other rule interpretable"
                            .to_owned(),
                    ],
                });
            }
            Some(value) => {
                let Some(version) = value.as_u64().filter(|version| *version >= 1) else {
                    return Err(Rejection {
                        code: Code::CatalogInvalid,
                        problems: vec!["catalog_version must be a positive integer".to_owned()],
                    });
                };
                if !SUPPORTED_VERSIONS.contains(&version) {
                    return Err(Rejection {
                        code: Code::CatalogVersionUnsupported,
                        problems: vec![format!(
                            "catalog_version {version} is not one this build reads; supported: {SUPPORTED_VERSIONS:?}"
                        )],
                    });
                }
            }
        }

        let mut problems = Vec::new();

        let databases = match object.get("databases") {
            None => {
                problems.push(
                    "databases is absent, and an absent member is indistinguishable from a truncated file"
                        .to_owned(),
                );
                None
            }
            Some(Value::Object(map)) => Some(map),
            Some(_) => {
                problems.push(
                    "databases must be an object, because the name is the key and an array would allow two entries claiming one name"
                        .to_owned(),
                );
                None
            }
        };

        let mut entries = BTreeMap::new();
        if let Some(map) = databases {
            for (name, value) in map {
                if let Some(problem) = name_problem(name) {
                    problems.push(problem);
                    continue;
                }
                match entry_from(value) {
                    Ok(entry) => {
                        entries.insert(name.clone(), entry);
                    }
                    Err(problem) => problems.push(format!("{name}: {problem}")),
                }
            }
        }

        if problems.is_empty() {
            let mut unknown = object.clone();
            unknown.remove("catalog_version");
            unknown.remove("databases");
            Ok(Self {
                databases: entries,
                unknown,
            })
        } else {
            Err(Rejection {
                code: Code::CatalogInvalid,
                problems,
            })
        }
    }

    /// The names in the catalog, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.databases.keys().map(String::as_str)
    }

    /// The entry a name refers to, if the catalog has one.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.databases.get(name)
    }

    /// Whether the catalog holds no names.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.databases.is_empty()
    }

    /// Adds or replaces a name.
    ///
    /// # Errors
    ///
    /// Returns a [`Rejection`] when the name or the path breaks the contract. The target is
    /// not opened, because a name may legitimately be registered for a database on a disk
    /// that is not currently mounted.
    pub fn insert(&mut self, name: &str, path: &Path) -> Result<(), Rejection> {
        let mut problems = Vec::new();
        if let Some(problem) = name_problem(name) {
            problems.push(problem);
        }
        if let Some(problem) = path_problem(path) {
            problems.push(format!("{name}: {problem}"));
        }
        if !problems.is_empty() {
            return Err(Rejection {
                code: Code::CatalogInvalid,
                problems,
            });
        }

        let unknown = self
            .databases
            .get(name)
            .map(|existing| existing.unknown.clone())
            .unwrap_or_default();
        self.databases.insert(
            name.to_owned(),
            Entry {
                path: path.to_path_buf(),
                unknown,
            },
        );
        Ok(())
    }

    /// Removes a name, reporting whether it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.databases.remove(name).is_some()
    }

    /// The document this catalog serializes to.
    #[must_use]
    pub fn to_document(&self) -> Value {
        let mut databases = Map::new();
        for (name, entry) in &self.databases {
            let mut object = entry.unknown.clone();
            object.insert(
                "path".to_owned(),
                Value::String(entry.path.display().to_string()),
            );
            databases.insert(name.clone(), Value::Object(object));
        }

        let mut root = Map::new();
        root.insert(
            "catalog_version".to_owned(),
            Value::Number(CURRENT_VERSION.into()),
        );
        root.insert("databases".to_owned(), Value::Object(databases));
        for (key, value) in &self.unknown {
            root.insert(key.clone(), value.clone());
        }
        Value::Object(root)
    }

    /// Writes the catalog to `path` as a complete replacement.
    ///
    /// The document is written to a temporary file beside the target and renamed over it, so a
    /// concurrent reader sees either the previous catalog or this one. The contract's section 5
    /// requires that, and requires a reader that does find a truncated document to refuse it
    /// rather than treat the readable prefix as the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory cannot be created, the temporary file cannot be
    /// written, or the rename fails. A failed write leaves the previous catalog in place.
    pub fn store(&self, path: &Path) -> Result<(), Error> {
        let directory = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the catalog path has no parent directory",
            )
        })?;
        fs::create_dir_all(directory)?;

        let mut text = serde_json::to_string_pretty(&self.to_document())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        text.push('\n');

        // The temporary name carries this process's id so two concurrent writers do not write
        // to one temporary file. The rename is what serializes them: the last complete write
        // wins, and neither reader ever sees a concatenation.
        let temporary = directory.join(format!(".catalog.json.{}.partial", std::process::id()));
        fs::write(&temporary, text.as_bytes())?;
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Leaving a partial file behind would look like a catalog to nothing, but it
                // would accumulate on every failure.
                let _ = fs::remove_file(&temporary);
                Err(Error::Io(error))
            }
        }
    }
}

/// The name form in the contract's section 3.2, or the reason it is refused.
fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a name is not empty".to_owned());
    }
    if name.starts_with('@') {
        return Some(format!(
            "{name}: the @ sigil belongs to the command line, not to the name"
        ));
    }
    let mut characters = name.chars();
    let first = characters.next()?;
    if !first.is_ascii_alphanumeric() {
        return Some(format!(
            "{name}: a name starts with an ASCII letter or digit"
        ));
    }
    if let Some(bad) = characters.find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-')) {
        return Some(format!(
            "{name}: {bad:?} is not allowed in a name, which excludes path separators and whitespace so a name is never mistaken for a path"
        ));
    }
    None
}

/// The path rule in the contract's section 3.3, or the reason it is refused.
fn path_problem(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return Some("path is empty".to_owned());
    }
    if !path.is_absolute() {
        return Some(format!(
            "path {} is relative, and a catalog is resolved from an arbitrary working directory, so a relative path has no anchor",
            path.display()
        ));
    }
    None
}

fn entry_from(value: &Value) -> Result<Entry, String> {
    let Some(object) = value.as_object() else {
        return Err(
            "an entry is an object, so a bare value has no room for the members a later version adds"
                .to_owned(),
        );
    };
    let Some(raw) = object.get("path") else {
        return Err(
            "path is absent, and an entry with no target is not a claim about anything".to_owned(),
        );
    };
    let Some(text) = raw.as_str() else {
        return Err("path is a string".to_owned());
    };
    let path = PathBuf::from(text);
    if let Some(problem) = path_problem(&path) {
        return Err(problem);
    }

    let mut unknown = object.clone();
    unknown.remove("path");
    Ok(Entry { path, unknown })
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_VERSION, Catalog};
    use crate::diagnostic::Code;
    use std::path::{Path, PathBuf};

    fn temporary_directory(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nostdb-server-{}-{label}-{}",
            std::process::id(),
            CURRENT_VERSION
        ));
        std::fs::create_dir_all(&base).expect("temporary directory");
        base
    }

    #[test]
    fn an_absent_catalog_reads_as_empty() {
        let path = temporary_directory("absent").join("catalog.json");
        let _ = std::fs::remove_file(&path);
        let catalog = Catalog::load(&path).expect("an absent catalog is not an error");
        assert!(catalog.is_empty());
    }

    #[test]
    fn a_round_trip_preserves_an_unknown_member() {
        let text = r#"{
          "catalog_version": 1,
          "databases": { "work": { "path": "/srv/work.nostdb", "colour": "blue" } },
          "written_by": "a newer build"
        }"#;
        let catalog = Catalog::parse(text).expect("accepted");
        let document = catalog.to_document();
        assert_eq!(document["written_by"], "a newer build");
        assert_eq!(document["databases"]["work"]["colour"], "blue");
        assert_eq!(document["databases"]["work"]["path"], "/srv/work.nostdb");
    }

    #[test]
    fn a_rewrite_after_removing_another_name_keeps_an_unknown_member() {
        // The preservation rule exists so an older build is not a silent downgrade. The way
        // that breaks in practice is a mutation, not a read, so this exercises one.
        let text = r#"{
          "catalog_version": 1,
          "databases": {
            "work": { "path": "/srv/work.nostdb", "colour": "blue" },
            "spare": { "path": "/srv/spare.nostdb" }
          },
          "written_by": "a newer build"
        }"#;
        let mut catalog = Catalog::parse(text).expect("accepted");
        assert!(catalog.remove("spare"));
        let document = catalog.to_document();
        assert_eq!(document["written_by"], "a newer build");
        assert_eq!(document["databases"]["work"]["colour"], "blue");
        assert!(document["databases"].get("spare").is_none());
    }

    #[test]
    fn a_store_then_load_round_trips() {
        let path = temporary_directory("round-trip").join("catalog.json");
        let mut catalog = Catalog::default();
        catalog
            .insert("work", Path::new("/srv/work.nostdb"))
            .expect("a valid name and an absolute path");
        catalog.store(&path).expect("stored");

        let reloaded = Catalog::load(&path).expect("read back");
        assert_eq!(
            reloaded.get("work").map(super::Entry::path),
            Some(Path::new("/srv/work.nostdb"))
        );
        assert_eq!(reloaded, catalog);
    }

    #[test]
    fn a_store_leaves_no_partial_file_behind() {
        let directory = temporary_directory("no-partial");
        let path = directory.join("catalog.json");
        Catalog::default().store(&path).expect("stored");
        let leftovers: Vec<_> = std::fs::read_dir(&directory)
            .expect("listing")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| name.to_string_lossy().contains("partial"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a partial file survived: {leftovers:?}"
        );
    }

    #[test]
    fn a_truncated_catalog_is_refused_rather_than_read_as_far_as_it_goes() {
        // What an interrupted write from an older build, or a full disk, leaves behind. Section 5
        // requires a reader that finds one to refuse it rather than treat the readable prefix as
        // the catalog: half a catalog is a set of names somebody would then act on.
        let path = temporary_directory("truncated").join("catalog.json");
        std::fs::write(
            &path,
            b"{\"catalog_version\": 1, \"databases\": {\"work\": {\"pat",
        )
        .expect("wrote a truncated catalog");

        let error = Catalog::load(&path).expect_err("refused");
        match error {
            super::Error::Rejected(rejection) => {
                assert_eq!(rejection.code(), Code::CatalogInvalid);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_catalog_survives_a_write_that_replaces_it() {
        // Recovery in the sense that matters here: the previous catalog is intact until the new one
        // is complete, because a write is a rename over the target rather than an edit in place.
        let path = temporary_directory("replace").join("catalog.json");
        let mut first = Catalog::default();
        first
            .insert("work", Path::new("/srv/a.nostdb"))
            .expect("one");
        first.store(&path).expect("stored");

        let mut second = Catalog::load(&path).expect("loaded");
        second
            .insert("spare", Path::new("/srv/b.nostdb"))
            .expect("two");
        second.store(&path).expect("stored again");

        let reloaded = Catalog::load(&path).expect("reloaded");
        assert_eq!(reloaded.names().collect::<Vec<_>>(), ["spare", "work"]);
    }

    #[test]
    fn an_unsupported_version_is_its_own_code() {
        let rejection =
            Catalog::parse(r#"{"catalog_version": 2, "databases": {}}"#).expect_err("refused");
        assert_eq!(rejection.code(), Code::CatalogVersionUnsupported);
    }

    #[test]
    fn a_missing_version_is_not_the_unsupported_code() {
        // These are different failures for the caller: one is a document that says nothing
        // about its version, the other is a version this build will not read.
        let rejection = Catalog::parse(r#"{"databases": {}}"#).expect_err("refused");
        assert_eq!(rejection.code(), Code::CatalogInvalid);
    }

    #[test]
    fn every_problem_is_reported_rather_than_the_first() {
        let text = r#"{
          "catalog_version": 1,
          "databases": {
            "work/main": { "path": "/srv/a.nostdb" },
            "@spare": { "path": "/srv/b.nostdb" },
            "third": { "path": "relative.nostdb" }
          }
        }"#;
        let rejection = Catalog::parse(text).expect_err("refused");
        assert_eq!(rejection.code(), Code::CatalogInvalid);
        assert_eq!(
            rejection.problems().len(),
            3,
            "a hand-edited catalog should be fixable in one pass: {:?}",
            rejection.problems()
        );
    }

    #[test]
    fn a_stale_target_is_not_a_catalog_problem() {
        // The rule the contract states before the shape, because it is the one most likely to
        // be implemented the other way by someone validating member by member.
        let text = r#"{
          "catalog_version": 1,
          "databases": { "work": { "path": "/nonexistent/disk/work.nostdb" } }
        }"#;
        let catalog = Catalog::parse(text).expect("a stale target is still a valid catalog");
        assert_eq!(
            catalog.get("work").map(super::Entry::path),
            Some(Path::new("/nonexistent/disk/work.nostdb"))
        );
    }

    #[test]
    fn insert_refuses_a_relative_path_without_touching_the_filesystem() {
        let mut catalog = Catalog::default();
        let rejection = catalog
            .insert("work", Path::new("./work.nostdb"))
            .expect_err("refused");
        assert_eq!(rejection.code(), Code::CatalogInvalid);
        assert!(catalog.is_empty(), "a refused insert changed the catalog");
    }

    #[test]
    fn insert_refuses_a_name_holding_a_path_separator() {
        let mut catalog = Catalog::default();
        assert!(
            catalog
                .insert("work/main", Path::new("/srv/a.nostdb"))
                .is_err()
        );
        assert!(catalog.insert("@work", Path::new("/srv/a.nostdb")).is_err());
        assert!(catalog.insert("", Path::new("/srv/a.nostdb")).is_err());
    }

    #[test]
    fn one_database_may_carry_two_names() {
        // A name is a nickname, not an identity, which the contract's section 1.2 states.
        let mut catalog = Catalog::default();
        catalog
            .insert("work", Path::new("/srv/w.nostdb"))
            .expect("first");
        catalog
            .insert("work-mirror", Path::new("/srv/w.nostdb"))
            .expect("second");
        assert_eq!(catalog.names().collect::<Vec<_>>(), ["work", "work-mirror"]);
    }

    #[test]
    fn an_empty_catalog_writes_an_empty_object_rather_than_omitting_the_member() {
        let document = Catalog::default().to_document();
        assert!(
            document["databases"].is_object(),
            "an absent member would be indistinguishable from a truncated file"
        );
        assert_eq!(document["catalog_version"], 1);
    }
}
