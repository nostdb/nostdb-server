#![forbid(unsafe_code)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nostos_server::config::DaemonConfig;
use nostos_server::daemon::DatabaseDaemon;
use tracing_subscriber::EnvFilter;

const HELP: &str = "NostosDB installable database daemon

Usage:
    nostosd init --data-dir PATH [--config PATH] [--listen IP:PORT]
    nostosd serve [--config PATH]
    nostosd --help
    nostosd --version

Initialization creates separate protected client and admin credential files.
Credential values are never accepted as command-line arguments.
The default database-protocol listener is 127.0.0.1:7878.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nostosd: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() || matches!(arguments[0].as_str(), "-h" | "--help") {
        println!("{HELP}");
        return Ok(());
    }
    if matches!(arguments[0].as_str(), "-V" | "--version") {
        if arguments.len() != 1 {
            return Err("--version does not accept arguments".to_owned());
        }
        println!("nostosd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let command = arguments.remove(0);
    match command.as_str() {
        "init" => initialize(parse_init(arguments)?),
        "serve" => serve(parse_serve(arguments)?).await,
        _ => Err(format!("unknown command `{command}`\n\n{HELP}")),
    }
}

struct InitOptions {
    data_directory: PathBuf,
    config_path: PathBuf,
    listen: String,
}

fn parse_init(arguments: Vec<String>) -> Result<InitOptions, String> {
    let mut data_directory = None;
    let mut config_path = None;
    let mut listen = "127.0.0.1:7878".to_owned();
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        match option.as_str() {
            "--data-dir" => data_directory = Some(value(&arguments, &mut index)?.into()),
            "--config" => config_path = Some(value(&arguments, &mut index)?.into()),
            "--listen" => listen = value(&arguments, &mut index)?.to_owned(),
            _ => return Err(format!("unknown init option `{option}`")),
        }
        index += 1;
    }
    let data_directory =
        data_directory.ok_or_else(|| "init requires --data-dir PATH".to_owned())?;
    let config_path = config_path.unwrap_or_else(default_config_path);
    Ok(InitOptions {
        data_directory,
        config_path,
        listen,
    })
}

fn parse_serve(arguments: Vec<String>) -> Result<PathBuf, String> {
    let mut config_path = None;
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        match option.as_str() {
            "--config" => config_path = Some(value(&arguments, &mut index)?.into()),
            _ => return Err(format!("unknown serve option `{option}`")),
        }
        index += 1;
    }
    Ok(config_path.unwrap_or_else(default_config_path))
}

fn value<'a>(arguments: &'a [String], index: &mut usize) -> Result<&'a str, String> {
    *index += 1;
    arguments
        .get(*index)
        .map(String::as_str)
        .ok_or_else(|| "option requires a value".to_owned())
}

fn initialize(options: InitOptions) -> Result<(), String> {
    let report = DatabaseDaemon::initialize(
        &options.config_path,
        &options.data_directory,
        &options.listen,
    )
    .map_err(|error| error.to_string())?;
    println!("initialized NostosDB data directory");
    println!("config: {}", report.config_path.display());
    println!("data: {}", report.data_directory.display());
    println!(
        "client credential: {}",
        report.query_credential_file.display()
    );
    println!(
        "admin credential: {}",
        report.admin_credential_file.display()
    );
    Ok(())
}

async fn serve(config_path: PathBuf) -> Result<(), String> {
    let config = DaemonConfig::load(&config_path).map_err(|error| error.to_string())?;
    let listen = config.listen_address().map_err(|error| error.to_string())?;
    init_tracing()?;
    let daemon = DatabaseDaemon::open(config).map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot bind database protocol listener {listen}: {error}"))?;
    tracing::info!(address = %listen, data_directory = %daemon.config().data_directory.display(), "NostosDB database daemon listening");
    nostos_server::serve_database_protocol(listener, daemon, shutdown_signal())
        .await
        .map_err(|error| error.to_string())
}

fn default_config_path() -> PathBuf {
    if let Some(path) = env::var_os("NOSTOS_CONFIG") {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    if let Some(program_data) = env::var_os("PROGRAMDATA") {
        return PathBuf::from(program_data).join("NostosDB/server.toml");
    }
    #[cfg(target_os = "macos")]
    if let Some(prefix) = env::var_os("HOMEBREW_PREFIX") {
        return PathBuf::from(prefix).join("etc/nostosdb/server.toml");
    }
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/etc/nostosdb/server.toml");
    }
    #[allow(unreachable_code)]
    env::current_dir()
        .unwrap_or_else(|_| Path::new(".").to_path_buf())
        .join("server.toml")
}

fn init_tracing() -> Result<(), String> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if env::var("NOSTOS_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .try_init()
            .map_err(|error| error.to_string())
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .try_init()
            .map_err(|error| error.to_string())
    }
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}
