//! The request loop: handshake, dispatch, and the transaction region.
//!
//! `server_protocol_version` 1 sections 4, 5, and 6 define what happens on a connection. This
//! module is generic over `Read + Write` rather than tied to a socket, so a test drives a whole
//! conversation over a pair of buffers or a `UnixStream` pair without a daemon running.
//!
//! # The transaction region
//!
//! `begin` enters a nested loop that reads this connection's messages until `commit` or
//! `rollback`. The Engine's `Transaction<'a>` borrows `&'a mut Database`, so the transaction
//! cannot outlive the scope holding it, and the nested loop *is* that scope. `nostdb-cli`'s REPL
//! reached the same shape for the same reason.
//!
//! The consequence is that the region owns the connection while it lasts, which is why section 6.1
//! gives a connection one session. Concurrency comes from more connections.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::Duration;

use nostdb_core::cancel::Deadline;
use nostdb_core::cypher;
use nostdb_core::execute::Parameters;
use nostdb_core::result::ResultEnvelope;
use nostdb_core::transaction::Transaction;
use serde_json::{Map, Value, json};

use crate::catalog::Catalog;
use crate::frame::{self, FrameError};
use crate::message::{self, Operation, Refusal, Request};
use crate::session::{OpenError, Slot};

/// The limits section 7 requires a build to enforce and make configurable.
///
/// `query_timeout` is enforced through the Engine's cooperative cancellation, which observes it at
/// part, clause, and match-row boundaries. `nostdb_core::cancel` states that granularity, and the
/// query subset contract's section 11.1 forbids claiming more of it than an implementation has: a
/// single Engine operation that does not yield between those boundaries runs to completion.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The largest frame this build accepts. At least [`frame::MINIMUM_MAXIMUM_FRAME_BYTES`].
    pub max_frame_bytes: u32,
    /// The largest number of rows a response carries.
    pub max_result_rows: usize,
    /// How long one query may run before it is asked to stop.
    pub query_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: frame::MINIMUM_MAXIMUM_FRAME_BYTES,
            max_result_rows: 100_000,
            query_timeout: Duration::from_secs(30),
        }
    }
}

/// How a connection ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The client closed the connection, or sent no more messages.
    ClientClosed,
    /// The handshake failed, so the connection was refused and closed.
    HandshakeRefused,
    /// The client asked the daemon to stop.
    ShutdownRequested,
    /// The transport failed.
    TransportFailed,
}

/// Serves one connection from the handshake to its end.
///
/// The catalog is re-read when a session opens rather than held, because `catalog add` in another
/// process changes it and a session opened afterwards should see the name.
///
/// # Errors
///
/// Never returns an error. Every failure is either a refusal sent to the client or an end recorded
/// in [`Ended`], because a connection failing is ordinary and must not take the daemon with it.
pub fn serve_connection<S: Read + Write>(
    stream: &mut S,
    catalog_path: &std::path::Path,
    endpoint: &str,
    limits: &Limits,
) -> Ended {
    // Section 4: the first message is the handshake, and nothing after it is interpretable until
    // a version is agreed.
    let opening = match frame::read_frame(stream, limits.max_frame_bytes) {
        Ok(body) => body,
        Err(FrameError::Closed) => return Ended::ClientClosed,
        Err(_) => return Ended::TransportFailed,
    };

    let version = match message::decode_hello(&opening).and_then(|hello| {
        let version = message::negotiate(&hello)?;
        Ok(version)
    }) {
        Ok(version) => version,
        Err(_) => {
            // A refusal states the supported set and closes. It names no negotiated version,
            // because there is none.
            let _ = send(stream, &message::refused(), limits);
            return Ended::HandshakeRefused;
        }
    };

    if send(stream, &message::welcome(version, endpoint), limits).is_err() {
        return Ended::TransportFailed;
    }

    let mut slot = Slot::new();
    let mut outstanding: BTreeSet<String> = BTreeSet::new();

    loop {
        let body = match frame::read_frame(stream, limits.max_frame_bytes) {
            Ok(body) => body,
            Err(FrameError::Closed) => {
                // Section 6.2: a dropped connection ends its session. Dropping the slot drops the
                // session and its database, and any transaction was a lexical region that has
                // already ended.
                slot.close();
                return Ended::ClientClosed;
            }
            Err(FrameError::TooLarge { declared, maximum }) => {
                // Refused from the prefix, before a buffer was sized. The connection is not
                // resynchronised, because the body was never read and the stream is now at an
                // unknown offset.
                let _ = send(
                    stream,
                    &json!({
                        "server_protocol_version": version,
                        "outcome": "error",
                        "rule": "frame_too_large",
                        "detail": format!(
                            "the peer declared a {declared}-byte frame, and the maximum is {maximum}"
                        ),
                    }),
                    limits,
                );
                return Ended::TransportFailed;
            }
            Err(_) => return Ended::TransportFailed,
        };

        let request = match message::decode_request(&body, &outstanding) {
            Ok(request) => request,
            Err(refusal) => {
                let _ = send(stream, &refusal_response(version, None, &refusal), limits);
                continue;
            }
        };

        outstanding.insert(request.request_id.clone());
        let reply = dispatch(&request, &mut slot, catalog_path, endpoint, limits, version);
        outstanding.remove(&request.request_id);

        match reply {
            Reply::Send(value) => {
                if send(stream, &value, limits).is_err() {
                    return Ended::TransportFailed;
                }
            }
            Reply::Transaction => {
                match run_transaction_region(stream, &mut slot, &request, limits, version) {
                    RegionEnd::Continue => {}
                    RegionEnd::ClientClosed => {
                        slot.close();
                        return Ended::ClientClosed;
                    }
                    RegionEnd::TransportFailed => return Ended::TransportFailed,
                }
            }
            Reply::Shutdown(value) => {
                let _ = send(stream, &value, limits);
                slot.close();
                return Ended::ShutdownRequested;
            }
        }
    }
}

enum Reply {
    Send(Value),
    /// `begin` was accepted, so the caller enters the transaction region.
    Transaction,
    Shutdown(Value),
}

fn dispatch(
    request: &Request,
    slot: &mut Slot,
    catalog_path: &std::path::Path,
    endpoint: &str,
    limits: &Limits,
    version: u64,
) -> Reply {
    match request.operation {
        Operation::OpenSession => {
            let Some(name) = &request.database else {
                return Reply::Send(error_response(
                    version,
                    request,
                    "open_session names the database to open",
                ));
            };
            let catalog = match Catalog::load(catalog_path) {
                Ok(catalog) => catalog,
                Err(error) => {
                    return Reply::Send(error_response(version, request, &format!("{error}")));
                }
            };
            match slot.open(name, &catalog) {
                Ok(id) => Reply::Send(ok_response(
                    version,
                    request,
                    json!({ "session_id": id.as_str() }),
                )),
                Err(OpenError::Refused(refusal)) => Reply::Send(refusal_response(
                    version,
                    Some(&request.request_id),
                    &refusal,
                )),
                Err(error) => Reply::Send(error_response(version, request, &format!("{error}"))),
            }
        }

        Operation::CloseSession => {
            let closed = slot.close();
            Reply::Send(ok_response(version, request, json!({ "closed": closed })))
        }

        Operation::Status => Reply::Send(ok_response(
            version,
            request,
            json!({
                "endpoint": endpoint,
                "sessions": usize::from(slot.is_open()),
                "server_protocol_version": version,
            }),
        )),

        Operation::Shutdown => Reply::Shutdown(ok_response(version, request, json!({}))),

        Operation::Query => match slot.resolve(request.session_id.as_deref()) {
            Err(refusal) => Reply::Send(refusal_response(
                version,
                Some(&request.request_id),
                &refusal,
            )),
            Ok(session) => {
                let Some(statement) = &request.statement else {
                    return Reply::Send(error_response(
                        version,
                        request,
                        "a query carries a statement",
                    ));
                };
                Reply::Send(run_one(
                    session.database_mut(),
                    statement,
                    request,
                    limits,
                    version,
                ))
            }
        },

        Operation::Begin => match slot.resolve(request.session_id.as_deref()) {
            Err(refusal) => Reply::Send(refusal_response(
                version,
                Some(&request.request_id),
                &refusal,
            )),
            Ok(_) => Reply::Transaction,
        },

        // Outside a region there is nothing to finish. This is a well-formed message in the wrong
        // state rather than a protocol violation, so it is an `error` outcome and not a section 8
        // refusal: the client can act on it, which is what separates the two.
        Operation::Commit | Operation::Rollback => Reply::Send(error_response(
            version,
            request,
            "there is no open transaction; send begin first",
        )),
    }
}

enum RegionEnd {
    Continue,
    ClientClosed,
    TransportFailed,
}

/// The transaction region: a nested loop owning the transaction until it ends.
///
/// This is the scope the Engine's borrow requires. Nothing outside it can hold the transaction, so
/// there is no state in which the daemon believes a transaction is open and it is not.
fn run_transaction_region<S: Read + Write>(
    stream: &mut S,
    slot: &mut Slot,
    begin: &Request,
    limits: &Limits,
    version: u64,
) -> RegionEnd {
    let session = match slot.resolve(begin.session_id.as_deref()) {
        Ok(session) => session,
        Err(refusal) => {
            let _ = send(
                stream,
                &refusal_response(version, Some(&begin.request_id), &refusal),
                limits,
            );
            return RegionEnd::Continue;
        }
    };

    let mut transaction = match Transaction::begin(session.database_mut()) {
        Ok(transaction) => transaction,
        Err(error) => {
            let _ = send(
                stream,
                &error_response(version, begin, &format!("{error}")),
                limits,
            );
            return RegionEnd::Continue;
        }
    };

    let base = transaction.base_generation();
    if send(
        stream,
        &ok_response(version, begin, json!({ "base_generation": base.get() })),
        limits,
    )
    .is_err()
    {
        transaction.rollback();
        return RegionEnd::TransportFailed;
    }

    let mut outstanding: BTreeSet<String> = BTreeSet::new();

    loop {
        let body = match frame::read_frame(stream, limits.max_frame_bytes) {
            Ok(body) => body,
            Err(FrameError::Closed) => {
                // Section 6.2: a client that disconnects mid-transaction has not decided to
                // commit, and treating a dropped connection as consent is how a partial write
                // becomes permanent.
                transaction.rollback();
                return RegionEnd::ClientClosed;
            }
            Err(_) => {
                transaction.rollback();
                return RegionEnd::TransportFailed;
            }
        };

        let request = match message::decode_request(&body, &outstanding) {
            Ok(request) => request,
            Err(refusal) => {
                if send(stream, &refusal_response(version, None, &refusal), limits).is_err() {
                    transaction.rollback();
                    return RegionEnd::TransportFailed;
                }
                continue;
            }
        };
        outstanding.insert(request.request_id.clone());

        let reply = match request.operation {
            Operation::Commit => match transaction.commit() {
                Ok(generation) => {
                    let _ = send(
                        stream,
                        &ok_response(version, &request, json!({ "generation": generation.get() })),
                        limits,
                    );
                    return RegionEnd::Continue;
                }
                Err(error) => {
                    // A refused commit ends the region: the transaction was consumed by the
                    // attempt, and the last valid generation is untouched.
                    let _ = send(
                        stream,
                        &error_response(version, &request, &format!("{error}")),
                        limits,
                    );
                    return RegionEnd::Continue;
                }
            },

            Operation::Rollback => {
                transaction.rollback();
                let _ = send(
                    stream,
                    &ok_response(version, &request, json!({ "rolled_back": true })),
                    limits,
                );
                return RegionEnd::Continue;
            }

            Operation::Query => {
                let Some(statement) = &request.statement else {
                    error_response(version, &request, "a query carries a statement")
                        .pipe_send(stream, limits);
                    continue;
                };
                match cypher::parse(statement) {
                    Err(error) => diagnostic_response(version, &request, &error),
                    Ok(query) => match transaction.run_cancellable(
                        &query,
                        &Parameters::new(),
                        &Deadline::after(limits.query_timeout),
                    ) {
                        Err(error) => diagnostic_response(version, &request, &error),
                        Ok(result) => {
                            let writes = transaction.writes();
                            envelope_response(version, &request, result, base, Some(writes), limits)
                        }
                    },
                }
            }

            // A nested `begin` would mean two transactions in one session, which section 6.2
            // rules out. Refusing it is what keeps the region's extent equal to the transaction's.
            Operation::Begin => error_response(
                version,
                &request,
                "this session already has an open transaction; commit or roll back first",
            ),

            // Ending the session or the daemon while a transaction is open would decide the
            // transaction's fate on the client's behalf.
            Operation::OpenSession | Operation::CloseSession | Operation::Shutdown => {
                error_response(
                    version,
                    &request,
                    "finish the open transaction before changing the session or stopping the daemon",
                )
            }

            Operation::Status => ok_response(
                version,
                &request,
                json!({ "in_transaction": true, "base_generation": base.get() }),
            ),
        };

        outstanding.remove(&request.request_id);
        if send(stream, &reply, limits).is_err() {
            transaction.rollback();
            return RegionEnd::TransportFailed;
        }
    }
}

/// Runs one statement in its own transaction, committing if it changed anything.
fn run_one(
    database: &mut nostdb_core::storage::Database,
    statement: &str,
    request: &Request,
    limits: &Limits,
    version: u64,
) -> Value {
    let query = match cypher::parse(statement) {
        Ok(query) => query,
        Err(error) => return diagnostic_response(version, request, &error),
    };

    let mut transaction = match Transaction::begin(database) {
        Ok(transaction) => transaction,
        Err(error) => return error_response(version, request, &format!("{error}")),
    };
    let base = transaction.base_generation();

    let result = match transaction.run_cancellable(
        &query,
        &Parameters::new(),
        &Deadline::after(limits.query_timeout),
    ) {
        Ok(result) => result,
        Err(error) => {
            transaction.rollback();
            return diagnostic_response(version, request, &error);
        }
    };

    let writes = transaction.writes();
    let wrote = !writes.is_empty();
    let generation = if wrote {
        match transaction.commit() {
            Ok(generation) => generation,
            Err(error) => return error_response(version, request, &format!("{error}")),
        }
    } else {
        transaction.rollback();
        base
    };

    envelope_response(
        version,
        request,
        result,
        generation,
        wrote.then_some(writes),
        limits,
    )
}

fn envelope_response(
    version: u64,
    request: &Request,
    result: nostdb_core::execute::QueryResult,
    generation: nostdb_core::generation::Generation,
    writes: Option<nostdb_core::WriteSummary>,
    limits: &Limits,
) -> Value {
    // Section 7: a request stopped by a limit reports which limit stopped it. This one is checked
    // after execution, because the Engine produces a whole result; it bounds what crosses the
    // socket rather than what was computed, and the root progress record says so.
    if result.row_count() > limits.max_result_rows {
        return json!({
            "server_protocol_version": version,
            "request_id": request.request_id,
            "outcome": "error",
            "limit": "max_result_rows",
            "detail": format!(
                "the query produced {} rows, and the per-session ceiling is {}",
                result.row_count(),
                limits.max_result_rows
            ),
        });
    }

    // Section 5.3: the envelope the Engine built, carried verbatim. This daemon states no field of
    // it, because a second assembler of a published shape is two that drift.
    let envelope = ResultEnvelope::new(result, generation, writes);
    json!({
        "server_protocol_version": version,
        "request_id": request.request_id,
        "outcome": "ok",
        "result": envelope.to_json(),
    })
}

fn ok_response(version: u64, request: &Request, result: Value) -> Value {
    json!({
        "server_protocol_version": version,
        "request_id": request.request_id,
        "outcome": "ok",
        "result": result,
    })
}

fn error_response(version: u64, request: &Request, detail: &str) -> Value {
    json!({
        "server_protocol_version": version,
        "request_id": request.request_id,
        "outcome": "error",
        "detail": detail,
    })
}

/// A failure the Engine named, forwarded with its own code.
///
/// Section 5.3 forbids translating an Engine code into one of the daemon's, and forbids adding a
/// code for a failure the Engine already named.
fn diagnostic_response(version: u64, request: &Request, error: &cypher::QueryError) -> Value {
    let mut object = Map::new();
    object.insert(
        "server_protocol_version".to_owned(),
        Value::Number(version.into()),
    );
    object.insert(
        "request_id".to_owned(),
        Value::String(request.request_id.clone()),
    );
    object.insert("outcome".to_owned(), Value::String("error".to_owned()));
    object.insert(
        "diagnostics".to_owned(),
        Value::Array(vec![json!({
            "code": error.code.as_str(),
            "message": error.message.clone(),
        })]),
    );
    Value::Object(object)
}

fn refusal_response(version: u64, request_id: Option<&str>, refusal: &Refusal) -> Value {
    let mut object = Map::new();
    object.insert(
        "server_protocol_version".to_owned(),
        Value::Number(version.into()),
    );
    if let Some(id) = request_id {
        object.insert("request_id".to_owned(), Value::String(id.to_owned()));
    }
    object.insert("outcome".to_owned(), Value::String("error".to_owned()));
    object.insert(
        "rule".to_owned(),
        Value::String(refusal.rule().as_str().to_owned()),
    );
    if let Some(code) = refusal.code() {
        object.insert("code".to_owned(), Value::String(code.as_str().to_owned()));
    }
    object.insert(
        "detail".to_owned(),
        Value::String(refusal.detail().to_owned()),
    );
    Value::Object(object)
}

fn send<S: Write>(stream: &mut S, value: &Value, limits: &Limits) -> Result<(), FrameError> {
    let body = serde_json::to_string(value).map_err(|error| {
        FrameError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    frame::write_frame(stream, &body, limits.max_frame_bytes)
}

/// A small helper so a branch that must both send and continue reads in one line.
trait PipeSend {
    fn pipe_send<S: Write>(&self, stream: &mut S, limits: &Limits);
}

impl PipeSend for Value {
    fn pipe_send<S: Write>(&self, stream: &mut S, limits: &Limits) {
        let _ = send(stream, self, limits);
    }
}
