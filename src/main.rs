//! The daemon binary.
//!
//! The user-facing command surface is `nostdb server ...` in `nostdb-cli`, which is where
//! argument parsing, output formats, and exit classes belong. This binary is the process that
//! surface starts, and it takes only what a service manager needs.
//!
//! Increment 2 has no protocol loop, so `run` binds the endpoint, reports it, and exits rather
//! than accepting connections. That is deliberate: a loop that accepted a connection and did
//! nothing with it would look like a working daemon.

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str).unwrap_or("run");

    match command {
        "run" => run(),
        "status" => status(),
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("nostdb-server: unknown command {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

const USAGE: &str = "\
nostdb-server — the NostDB per-user local daemon

Usage:
  nostdb-server [run]      Acquire the single-instance lock and bind the endpoint
  nostdb-server status     Report whether a daemon is running for this user
  nostdb-server help       Show this text

The user-facing surface is `nostdb server ...`, which starts this process.
";

fn run() -> ExitCode {
    match nostdb_server::start() {
        Ok(nostdb_server::Started::Running { address, .. }) => {
            println!("listening on {}", address.display());
            ExitCode::SUCCESS
        }
        Ok(nostdb_server::Started::AlreadyRunning { address, code }) => {
            // Not a failure. Section 2.1 of the protocol contract makes a start that finds a
            // healthy daemon a success reporting the existing endpoint.
            println!("{code}: already listening on {}", address.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nostdb-server: {error}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    match nostdb_server::is_running() {
        Ok(true) => {
            println!("running");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("not running");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nostdb-server: {error}");
            ExitCode::FAILURE
        }
    }
}
