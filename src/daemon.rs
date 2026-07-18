//! Managed data-directory runtime used by the database protocol and operators.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use fs2::FileExt;
use nostos_client::{ClientRole, DatabaseDetails, DatabaseSummary, ErrorCode, WireQueryLimits};
use nostos_engine::{
    CancellationToken, DatabaseError, EmbeddedDatabase, Parameters, QueryErrorCode, QueryLimits,
    StatementResult, StorageErrorKind, prepare_write,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::catalog::{CatalogDatabase, CatalogStore, OperationKind, valid_database_name};
use crate::config::{Credentials, DaemonConfig, write_credential};
use crate::{ServerError, wire};

/// Non-secret paths created by `nostosd init`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializationReport {
    /// Newly written configuration file.
    pub config_path: PathBuf,
    /// Initialized daemon-owned data directory.
    pub data_directory: PathBuf,
    /// Ordinary client credential file. Its value is never returned here.
    pub query_credential_file: PathBuf,
    /// Administrative credential file. Its value is never returned here.
    pub admin_credential_file: PathBuf,
}

/// Stable failure sent through the database protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolFailure {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

impl ProtocolFailure {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub(crate) fn retryable(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: true,
        }
    }
}

pub(crate) struct ManagedDatabase {
    id: String,
    path: PathBuf,
    database: Mutex<Option<EmbeddedDatabase>>,
}

impl ManagedDatabase {
    fn new(id: String, path: PathBuf, database: EmbeddedDatabase) -> Self {
        Self {
            id,
            path,
            database: Mutex::new(Some(database)),
        }
    }
}

/// One running, exclusively owned data directory and its managed Databases.
pub struct DatabaseDaemon {
    config: Arc<DaemonConfig>,
    credentials: Credentials,
    catalog: Mutex<CatalogStore>,
    databases: RwLock<BTreeMap<String, Arc<ManagedDatabase>>>,
    _data_directory_lock: File,
}

impl DatabaseDaemon {
    /// Initializes a fresh data directory, protected credentials, and config.
    pub fn initialize(
        config_path: &Path,
        data_directory: &Path,
        listen: &str,
    ) -> Result<InitializationReport, ServerError> {
        let data_directory = absolute(data_directory)?;
        let config_path = absolute(config_path)?;
        let config = DaemonConfig::new(data_directory.clone(), listen.to_owned());
        config.listen_address()?;
        CatalogStore::initialize(&data_directory)?;

        let query_credential = generate_credential();
        let admin_credential = generate_credential();
        write_credential(
            &config.authentication.query_credential_file,
            &query_credential,
        )?;
        write_credential(
            &config.authentication.admin_credential_file,
            &admin_credential,
        )?;
        config.write_new(&config_path)?;
        Ok(InitializationReport {
            config_path,
            data_directory,
            query_credential_file: config.authentication.query_credential_file,
            admin_credential_file: config.authentication.admin_credential_file,
        })
    }

    /// Acquires exclusive ownership, recovers completed operations, and opens all Databases.
    pub fn open(config: DaemonConfig) -> Result<Arc<Self>, ServerError> {
        let data_lock = acquire_data_directory_lock(&config.data_directory)?;
        recover_snapshot_operations(&config.data_directory)?;
        let credentials = Credentials::load(&config)?;
        let catalog = CatalogStore::load(&config.data_directory)?;
        let mut databases = BTreeMap::new();
        for entry in &catalog.catalog().databases {
            let path = catalog.database_path(&entry.id);
            let database = EmbeddedDatabase::open(&path).map_err(|error| {
                ServerError::new(format!(
                    "cannot open managed Database `{}`: {error}",
                    entry.name
                ))
            })?;
            let info = database.info().map_err(|error| {
                ServerError::new(format!(
                    "cannot inspect managed Database `{}`: {error}",
                    entry.name
                ))
            })?;
            if info.source_managed {
                return Err(ServerError::new(format!(
                    "managed Database `{}` still has Source Mode authority; import it explicitly",
                    entry.name
                )));
            }
            databases.insert(
                entry.id.clone(),
                Arc::new(ManagedDatabase::new(entry.id.clone(), path, database)),
            );
        }
        Ok(Arc::new(Self {
            config: Arc::new(config),
            credentials,
            catalog: Mutex::new(catalog),
            databases: RwLock::new(databases),
            _data_directory_lock: data_lock,
        }))
    }

    /// Returns the immutable runtime configuration.
    #[must_use]
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub(crate) fn authenticate(&self, credential: &str) -> Option<ClientRole> {
        self.credentials.authenticate(credential)
    }

    pub(crate) fn list_databases(&self) -> Result<Vec<DatabaseSummary>, ProtocolFailure> {
        let catalog = self.catalog_lock()?;
        Ok(catalog
            .catalog()
            .databases
            .iter()
            .map(CatalogDatabase::summary)
            .collect())
    }

    pub(crate) fn select_database(&self, name: &str) -> Result<DatabaseSummary, ProtocolFailure> {
        let catalog = self.catalog_lock()?;
        catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .map(CatalogDatabase::summary)
            .ok_or_else(|| {
                ProtocolFailure::new(
                    ErrorCode::DatabaseNotFound,
                    format!("Database `{name}` does not exist"),
                )
            })
    }

    pub(crate) fn create_database(&self, name: &str) -> Result<DatabaseSummary, ProtocolFailure> {
        if !valid_database_name(name) {
            return Err(ProtocolFailure::new(
                ErrorCode::InvalidDatabaseName,
                "Database names must match [a-z][a-z0-9_-]{0,62}",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        if catalog
            .catalog()
            .databases
            .iter()
            .any(|entry| entry.name == name)
        {
            return Err(ProtocolFailure::new(
                ErrorCode::DatabaseAlreadyExists,
                format!("Database `{name}` already exists"),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let directory = catalog.database_directory(&id);
        fs::create_dir(&directory).map_err(internal)?;
        let path = catalog.database_path(&id);
        let database = match EmbeddedDatabase::create(&path) {
            Ok(database) => database,
            Err(error) => {
                let _ = fs::remove_dir(&directory);
                return Err(database_failure(error, None));
            }
        };
        let entry = CatalogDatabase {
            id: id.clone(),
            name: name.to_owned(),
            state: "ready".to_owned(),
        };
        let mut next = catalog.catalog().clone();
        next.databases.push(entry.clone());
        next.databases
            .sort_by(|left, right| left.name.cmp(&right.name));
        if let Err(error) = catalog.transition(next, OperationKind::Create, &id) {
            drop(database);
            let target = self
                .config
                .data_directory
                .join("recovery")
                .join(format!("failed-create-{id}"));
            let _ = fs::rename(&directory, target);
            return Err(internal(error));
        }
        self.databases_write()?.insert(
            id.clone(),
            Arc::new(ManagedDatabase::new(id, path, database)),
        );
        catalog.finish_transition().map_err(internal)?;
        Ok(entry.summary())
    }

    pub(crate) fn rename_database(
        &self,
        name: &str,
        new_name: &str,
    ) -> Result<DatabaseSummary, ProtocolFailure> {
        if !valid_database_name(new_name) {
            return Err(ProtocolFailure::new(
                ErrorCode::InvalidDatabaseName,
                "Database names must match [a-z][a-z0-9_-]{0,62}",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        if catalog
            .catalog()
            .databases
            .iter()
            .any(|entry| entry.name == new_name)
        {
            return Err(ProtocolFailure::new(
                ErrorCode::DatabaseAlreadyExists,
                format!("Database `{new_name}` already exists"),
            ));
        }
        let existing = catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))?;
        let mut next = catalog.catalog().clone();
        let entry = next
            .databases
            .iter_mut()
            .find(|entry| entry.id == existing.id)
            .expect("copied catalog contains the selected Database");
        entry.name = new_name.to_owned();
        let updated = entry.clone();
        next.databases
            .sort_by(|left, right| left.name.cmp(&right.name));
        catalog
            .transition(next, OperationKind::Rename, &existing.id)
            .map_err(internal)?;
        catalog.finish_transition().map_err(internal)?;
        Ok(updated.summary())
    }

    pub(crate) fn drop_database(
        &self,
        name: &str,
        confirm_name: &str,
    ) -> Result<DatabaseSummary, ProtocolFailure> {
        if name != confirm_name {
            return Err(ProtocolFailure::new(
                ErrorCode::ProtocolViolation,
                "confirm_name must exactly equal the Database name",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        let entry = catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))?;
        let mut databases = self.databases_write()?;
        let handle = databases
            .get(&entry.id)
            .cloned()
            .ok_or_else(|| internal("managed Database handle is missing"))?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let mut database = guard.take().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        database
            .checkpoint()
            .map_err(|error| database_failure(error, None))?;
        drop(database);
        drop(guard);
        databases.remove(&entry.id);
        drop(databases);

        let mut next = catalog.catalog().clone();
        next.databases.retain(|candidate| candidate.id != entry.id);
        if let Err(error) = catalog.transition(next, OperationKind::Drop, &entry.id) {
            let reopened = EmbeddedDatabase::open(&handle.path).map_err(|open_error| {
                internal(format!(
                    "catalog drop failed ({error}) and Database could not reopen: {open_error}"
                ))
            })?;
            *handle
                .database
                .lock()
                .map_err(|_| internal("managed Database lock is poisoned"))? = Some(reopened);
            self.databases_write()?.insert(entry.id.clone(), handle);
            return Err(internal(error));
        }
        let directory = catalog.database_directory(&entry.id);
        let trash = catalog.trash_directory(&entry.id);
        if trash.exists() {
            return Err(internal("Database trash target already exists"));
        }
        fs::rename(directory, trash).map_err(internal)?;
        catalog.finish_transition().map_err(internal)?;
        Ok(entry.summary())
    }

    pub(crate) fn inspect_database(&self, name: &str) -> Result<DatabaseDetails, ProtocolFailure> {
        let entry = self.entry(name)?;
        let handle = self.handle(&entry.id)?;
        let guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_ref().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let info = database
            .info()
            .map_err(|error| database_failure(error, None))?;
        let counts = database
            .counts()
            .map_err(|error| database_failure(error, None))?;
        let healthy = database
            .check()
            .map_err(|error| database_failure(error, None))?
            .is_valid();
        Ok(DatabaseDetails {
            summary: entry.summary(),
            ndb_format_version: info.ndb_format_version,
            schema_revision: info.schema_revision,
            generation: info.generation,
            logical_checksum: format!("{:016x}", info.logical_checksum),
            healthy,
            schemas: counts.schemas,
            nodes: counts.nodes,
            edges: counts.edges,
        })
    }

    pub(crate) fn execute(
        &self,
        database_name: &str,
        query: &str,
        parameters: BTreeMap<String, Value>,
        read_only: bool,
        requested_limits: Option<WireQueryLimits>,
        cancellation: CancellationToken,
    ) -> Result<StatementResult, ProtocolFailure> {
        if read_only && prepare_write(query).is_ok() {
            return Err(ProtocolFailure::new(
                ErrorCode::QueryError,
                "read_only request rejected a mutating query",
            ));
        }
        let parameters = wire::parameters(parameters)
            .map_err(|message| ProtocolFailure::new(ErrorCode::QueryError, message))?;
        let handle = self.handle_for_name(database_name)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let limits = lower_limits(self.config.query_limits(), requested_limits);
        database
            .execute_limited(query, &parameters, limits, cancellation.clone())
            .map_err(|error| database_failure(error, Some(&cancellation)))
    }

    pub(crate) fn execute_transaction(
        &self,
        database_name: &str,
        statements: Vec<(String, BTreeMap<String, Value>)>,
        cancellation: CancellationToken,
    ) -> Result<Vec<StatementResult>, ProtocolFailure> {
        let statements = statements
            .into_iter()
            .map(|(query, parameters)| {
                wire::parameters(parameters)
                    .map(|parameters| (query, parameters))
                    .map_err(|message| ProtocolFailure::new(ErrorCode::QueryError, message))
            })
            .collect::<Result<Vec<(String, Parameters)>, _>>()?;
        let handle = self.handle_for_name(database_name)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        database
            .execute_transaction_limited(
                &statements,
                self.config.query_limits(),
                cancellation.clone(),
            )
            .map_err(|error| database_failure(error, Some(&cancellation)))
    }

    pub(crate) fn export_snapshot(&self, name: &str) -> Result<Vec<u8>, ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        database
            .checkpoint()
            .map_err(|error| database_failure(error, None))?;
        fs::read(&handle.path).map_err(internal)
    }

    pub(crate) fn restore_snapshot(&self, name: &str, bytes: &[u8]) -> Result<(), ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        restore_snapshot(&handle, bytes)
    }

    pub(crate) fn export_logical(&self, name: &str) -> Result<Value, ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        let guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_ref().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let package = database
            .export_logical()
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        serde_json::to_value(LogicalPackageDocument::from(package)).map_err(internal)
    }

    pub(crate) fn import_logical(
        &self,
        name: &str,
        package: Value,
    ) -> Result<u64, ProtocolFailure> {
        let package: LogicalPackageDocument = serde_json::from_value(package).map_err(|error| {
            ProtocolFailure::new(
                ErrorCode::QueryError,
                format!("invalid logical package: {error}"),
            )
        })?;
        let modules = u64::try_from(package.modules.len())
            .map_err(|_| ProtocolFailure::new(ErrorCode::RequestTooLarge, "too many modules"))?;
        let handle = self.handle_for_name(name)?;
        import_logical(&handle, package)?;
        Ok(modules)
    }

    fn entry(&self, name: &str) -> Result<CatalogDatabase, ProtocolFailure> {
        self.catalog_lock()?
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))
    }

    fn handle_for_name(&self, name: &str) -> Result<Arc<ManagedDatabase>, ProtocolFailure> {
        let entry = self.entry(name)?;
        self.handle(&entry.id)
    }

    fn handle(&self, id: &str) -> Result<Arc<ManagedDatabase>, ProtocolFailure> {
        self.databases
            .read()
            .map_err(|_| internal("managed Database map is poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
            })
    }

    fn catalog_lock(&self) -> Result<std::sync::MutexGuard<'_, CatalogStore>, ProtocolFailure> {
        self.catalog
            .lock()
            .map_err(|_| internal("catalog lock is poisoned"))
    }

    fn databases_write(
        &self,
    ) -> Result<
        std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<ManagedDatabase>>>,
        ProtocolFailure,
    > {
        self.databases
            .write()
            .map_err(|_| internal("managed Database map is poisoned"))
    }
}

fn acquire_data_directory_lock(root: &Path) -> Result<File, ServerError> {
    let path = root.join("locks/daemon.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| {
            ServerError::new(format!(
                "cannot open data-directory lock {}: {error}",
                path.display()
            ))
        })?;
    FileExt::try_lock_exclusive(&file).map_err(|error| {
        ServerError::new(format!(
            "data directory {} is already owned by another daemon: {error}",
            root.display()
        ))
    })?;
    file.set_len(0)
        .map_err(|error| ServerError::new(error.to_string()))?;
    writeln!(file, "pid={}", std::process::id())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            ServerError::new(format!("cannot persist daemon lock metadata: {error}"))
        })?;
    Ok(file)
}

fn generate_credential() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn lower_limits(defaults: QueryLimits, requested: Option<WireQueryLimits>) -> QueryLimits {
    let Some(requested) = requested else {
        return defaults;
    };
    QueryLimits {
        max_rows: requested
            .max_rows
            .map_or(defaults.max_rows, |value| defaults.max_rows.min(value)),
        max_memory_bytes: requested
            .max_memory_bytes
            .map_or(defaults.max_memory_bytes, |value| {
                defaults.max_memory_bytes.min(value)
            }),
        max_operations: requested
            .max_operations
            .map_or(defaults.max_operations, |value| {
                defaults.max_operations.min(value)
            }),
        max_traversals: requested
            .max_traversals
            .map_or(defaults.max_traversals, |value| {
                defaults.max_traversals.min(value)
            }),
    }
}

fn database_failure(
    error: DatabaseError,
    cancellation: Option<&CancellationToken>,
) -> ProtocolFailure {
    match error {
        DatabaseError::Query(error) if error.code() == QueryErrorCode::ResourceLimit => {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                ProtocolFailure::new(ErrorCode::Cancelled, error.to_string())
            } else {
                ProtocolFailure::new(ErrorCode::ResourceLimit, error.to_string())
            }
        }
        DatabaseError::Query(error) => {
            ProtocolFailure::new(ErrorCode::QueryError, error.to_string())
        }
        DatabaseError::Storage(error) if error.kind() == StorageErrorKind::Busy => {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, error.to_string())
        }
        DatabaseError::Storage(error) => {
            ProtocolFailure::new(ErrorCode::InternalError, error.to_string())
        }
    }
}

fn not_found(name: &str) -> ProtocolFailure {
    ProtocolFailure::new(
        ErrorCode::DatabaseNotFound,
        format!("Database `{name}` does not exist"),
    )
}

fn internal(error: impl std::fmt::Display) -> ProtocolFailure {
    ProtocolFailure::new(ErrorCode::InternalError, error.to_string())
}

fn absolute(path: &Path) -> Result<PathBuf, ServerError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| ServerError::new(format!("cannot resolve current directory: {error}")))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalPackageDocument {
    package_version: u32,
    language_version: u32,
    config: String,
    modules: Vec<LogicalModuleDocument>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalModuleDocument {
    path: String,
    stable_module_id: String,
    source: String,
}

impl From<nostos_engine::LogicalPackage> for LogicalPackageDocument {
    fn from(package: nostos_engine::LogicalPackage) -> Self {
        Self {
            package_version: package.package_version,
            language_version: 1,
            config: package.config,
            modules: package
                .modules
                .into_iter()
                .map(|module| LogicalModuleDocument {
                    path: module.path,
                    stable_module_id: module.module_id,
                    source: module.source,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    operation_version: u32,
    database_id: String,
    stage: String,
}

fn restore_snapshot(handle: &ManagedDatabase, bytes: &[u8]) -> Result<(), ProtocolFailure> {
    if bytes.is_empty() {
        return Err(ProtocolFailure::new(
            ErrorCode::SnapshotIncompatible,
            "snapshot is empty",
        ));
    }
    let directory = handle
        .path
        .parent()
        .ok_or_else(|| internal("managed Database path has no parent"))?;
    let candidate = directory.join("database.ndb.restore-candidate");
    let backup = directory.join("database.ndb.restore-backup");
    let journal = directory.join("restore-operation");
    for path in [&candidate, &backup, &journal] {
        if path.exists() {
            return Err(ProtocolFailure::retryable(
                ErrorCode::RecoveryRequired,
                "a previous snapshot restore requires daemon restart recovery",
            ));
        }
    }
    let result = (|| {
        write_new_synced(&candidate, bytes)?;
        let mut proposed = EmbeddedDatabase::open(&candidate).map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        if !proposed
            .check()
            .map_err(|error| {
                ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
            })?
            .is_valid()
        {
            return Err(ProtocolFailure::new(
                ErrorCode::SnapshotIncompatible,
                "snapshot integrity check failed",
            ));
        }
        proposed.adopt_server_authority().map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        proposed.checkpoint().map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        drop(proposed);
        write_restore_journal(&journal, &handle.id, "prepared")?;

        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let mut current = guard.take().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        current
            .checkpoint()
            .map_err(|error| database_failure(error, None))?;
        drop(current);
        fs::rename(&handle.path, &backup).map_err(internal)?;
        if let Err(error) = fs::rename(&candidate, &handle.path) {
            let _ = fs::rename(&backup, &handle.path);
            *guard = EmbeddedDatabase::open(&handle.path).ok();
            let _ = fs::remove_file(&candidate);
            remove_sidecar(&candidate);
            let _ = fs::remove_file(&journal);
            return Err(internal(format!("cannot install snapshot: {error}")));
        }
        match EmbeddedDatabase::open(&handle.path) {
            Ok(database) => *guard = Some(database),
            Err(error) => {
                let _ = fs::remove_file(&handle.path);
                let _ = fs::rename(&backup, &handle.path);
                *guard = EmbeddedDatabase::open(&handle.path).ok();
                return Err(internal(format!(
                    "installed snapshot could not reopen: {error}"
                )));
            }
        }
        fs::remove_file(&backup).map_err(internal)?;
        remove_sidecar(&backup);
        fs::remove_file(&journal).map_err(internal)?;
        remove_sidecar(&candidate);
        Ok(())
    })();
    if result.is_err() && candidate.exists() && !journal.exists() {
        let _ = fs::remove_file(&candidate);
        remove_sidecar(&candidate);
    }
    result
}

fn recover_snapshot_operations(root: &Path) -> Result<(), ServerError> {
    let databases = root.join("databases");
    if !databases.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&databases)
        .map_err(|error| ServerError::new(format!("cannot inspect restore state: {error}")))?
    {
        let entry = entry.map_err(|error| ServerError::new(error.to_string()))?;
        if !entry.path().is_dir() {
            continue;
        }
        let directory = entry.path();
        let journal_path = directory.join("restore-operation");
        if !journal_path.exists() {
            continue;
        }
        let journal: RestoreJournal = serde_json::from_slice(
            &fs::read(&journal_path).map_err(|error| ServerError::new(error.to_string()))?,
        )
        .map_err(|error| ServerError::new(format!("invalid restore journal: {error}")))?;
        if journal.operation_version != 1
            || journal.database_id != entry.file_name().to_string_lossy()
        {
            return Err(ServerError::new(
                "restore journal identity or version is invalid; operator recovery is required",
            ));
        }
        let target = directory.join("database.ndb");
        let candidate = directory.join("database.ndb.restore-candidate");
        let backup = directory.join("database.ndb.restore-backup");
        if journal.stage != "prepared" {
            return Err(ServerError::new(
                "restore journal stage is invalid; operator recovery is required",
            ));
        }
        match (target.exists(), candidate.exists(), backup.exists()) {
            (true, true, false) => {
                remove_file_and_sidecar(&candidate)?;
            }
            (false, true, true) => {
                fs::rename(&candidate, &target).map_err(|error| {
                    ServerError::new(format!("cannot finish snapshot installation: {error}"))
                })?;
            }
            (true, false, true) | (true, false, false) => {}
            (false, false, true) => {
                fs::rename(&backup, &target).map_err(|error| {
                    ServerError::new(format!("cannot roll back snapshot restore: {error}"))
                })?;
            }
            _ => {
                return Err(ServerError::new(
                    "snapshot restore files are inconsistent; operator recovery is required",
                ));
            }
        }
        remove_file_and_sidecar(&candidate)?;
        remove_file_and_sidecar(&backup)?;
        fs::remove_file(journal_path).map_err(|error| ServerError::new(error.to_string()))?;
    }
    Ok(())
}

fn import_logical(
    handle: &ManagedDatabase,
    package: LogicalPackageDocument,
) -> Result<(), ProtocolFailure> {
    if package.package_version != 1 || package.language_version != 1 {
        return Err(ProtocolFailure::new(
            ErrorCode::QueryError,
            "logical package and language versions must both be 1",
        ));
    }
    let parent = handle
        .path
        .parent()
        .ok_or_else(|| internal("managed Database path has no parent"))?;
    let source_root = parent.join(format!("logical-import-{}", Uuid::new_v4()));
    fs::create_dir(&source_root).map_err(internal)?;
    let result = (|| {
        fs::write(source_root.join("nostos.toml"), package.config).map_err(internal)?;
        let config = nostos_engine::ProjectConfig::load(&source_root)
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        let mut seen = std::collections::BTreeSet::new();
        for module in package.modules {
            let relative = safe_module_path(&module.path)?;
            if !seen.insert(relative.clone()) {
                return Err(ProtocolFailure::new(
                    ErrorCode::QueryError,
                    "logical package repeats a module path",
                ));
            }
            let module_id = module
                .stable_module_id
                .parse::<nostos_engine::StableModuleId>()
                .map_err(|_| {
                    ProtocolFailure::new(ErrorCode::QueryError, "invalid stable Module ID")
                })?;
            if config.module_id(&relative) != Some(module_id) {
                return Err(ProtocolFailure::new(
                    ErrorCode::QueryError,
                    format!(
                        "stable Module ID does not match nostos.toml for {}",
                        module.path
                    ),
                ));
            }
            let path = source_root.join(&relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(internal)?;
            }
            fs::write(path, module.source).map_err(internal)?;
        }
        let candidate = source_root.join("candidate.ndb");
        nostos_engine::Synchronizer::default()
            .sync(&source_root, &candidate)
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        let bytes = fs::read(&candidate).map_err(internal)?;
        restore_snapshot(handle, &bytes)
    })();
    if result.is_ok() {
        fs::remove_dir_all(&source_root).map_err(internal)?;
    }
    result
}

fn safe_module_path(value: &str) -> Result<PathBuf, ProtocolFailure> {
    use std::path::Component;
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.extension().and_then(|extension| extension.to_str()) != Some("nostos")
    {
        return Err(ProtocolFailure::new(
            ErrorCode::QueryError,
            format!("invalid logical module path `{value}`"),
        ));
    }
    Ok(path.to_path_buf())
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), ProtocolFailure> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(internal)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(internal)
}

fn write_restore_journal(path: &Path, id: &str, stage: &str) -> Result<(), ProtocolFailure> {
    let journal = RestoreJournal {
        operation_version: 1,
        database_id: id.to_owned(),
        stage: stage.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&journal).map_err(internal)?;
    write_new_synced(path, &bytes)
}

fn remove_file_and_sidecar(path: &Path) -> Result<(), ServerError> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| ServerError::new(error.to_string()))?;
    }
    remove_sidecar(path);
    Ok(())
}

fn remove_sidecar(path: &Path) {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    let _ = fs::remove_file(PathBuf::from(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_limits_can_only_lower_server_limits() {
        let defaults = QueryLimits {
            max_rows: 10,
            max_memory_bytes: 20,
            max_operations: 30,
            max_traversals: 40,
        };
        let lowered = lower_limits(
            defaults,
            Some(WireQueryLimits {
                max_rows: Some(5),
                max_memory_bytes: Some(200),
                max_operations: None,
                max_traversals: Some(0),
            }),
        );
        assert_eq!(lowered.max_rows, 5);
        assert_eq!(lowered.max_memory_bytes, 20);
        assert_eq!(lowered.max_operations, 30);
        assert_eq!(lowered.max_traversals, 0);
    }
}
