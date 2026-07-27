//! Whole conversations over a real socket pair.
//!
//! These drive `serve_connection` end to end: handshake, session, query, transaction region, and
//! the ways each can fail. A unit test on the decoder proves a message is refused; only a
//! conversation proves the daemon then stays usable, rolls a transaction back, or ends the session.
//!
//! The daemon runs on a thread and the test is the client, because the request loop reads until the
//! client closes and a single-threaded test would deadlock on its own first write.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nostdb_server::catalog::Catalog;
use nostdb_server::serve::{Ended, Limits, serve_connection};
use serde_json::{Value, json};

const MAX: u32 = 8 * 1024 * 1024;

fn scratch(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "nostdb-server-conversation-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).expect("scratch directory");
    base
}

/// A catalog naming one real, empty database, and the catalog's own path.
fn fixture(label: &str) -> (PathBuf, PathBuf) {
    let directory = scratch(label);
    let database = directory.join("root.nostdb");
    let _ = std::fs::remove_file(&database);
    nostdb_core::storage::Database::create(&database).expect("created");

    let mut catalog = Catalog::default();
    catalog.insert("work", &database).expect("registered");
    let catalog_path = directory.join("catalog.json");
    catalog.store(&catalog_path).expect("stored");
    (catalog_path, database)
}

/// A client end, with the daemon serving the other end on a thread.
struct Client {
    stream: UnixStream,
    daemon: Option<std::thread::JoinHandle<Ended>>,
}

impl Client {
    fn connect(catalog_path: &Path, limits: Limits) -> Self {
        let (client, mut server) = UnixStream::pair().expect("socket pair");
        let catalog_path = catalog_path.to_path_buf();
        let daemon = std::thread::spawn(move || {
            serve_connection(&mut server, &catalog_path, "/test/endpoint", &limits)
        });
        Self {
            stream: client,
            daemon: Some(daemon),
        }
    }

    fn send(&mut self, value: &Value) {
        let body = serde_json::to_string(value).expect("serialized");
        nostdb_server::frame::write_frame(&mut self.stream, &body, MAX).expect("written");
    }

    fn receive(&mut self) -> Value {
        let body = nostdb_server::frame::read_frame(&mut self.stream, MAX).expect("a reply");
        serde_json::from_str(&body).expect("a JSON reply")
    }

    /// Sends `hello` and returns the reply, without asserting what it is.
    fn shake(&mut self, versions: Vec<u64>) -> Value {
        self.send(&json!({
            "message": "hello",
            "client": "conversation-test",
            "supported_versions": versions,
        }));
        self.receive()
    }

    /// The usual opening: negotiate version 1 and open a session on `work`.
    fn opened(catalog_path: &Path) -> (Self, String) {
        let mut client = Self::connect(catalog_path, Limits::default());
        let welcome = client.shake(vec![1]);
        assert_eq!(welcome["message"], "welcome");
        assert_eq!(welcome["server_protocol_version"], 1);

        client.send(&json!({
            "server_protocol_version": 1,
            "request_id": "open",
            "operation": "open_session",
            "database": "work",
        }));
        let reply = client.receive();
        assert_eq!(reply["outcome"], "ok", "{reply}");
        let id = reply["result"]["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned();
        (client, id)
    }

    /// Closes the client end and returns how the daemon saw the connection end.
    fn finish(mut self) -> Ended {
        let stream = self.stream.try_clone().expect("clone");
        drop(stream);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.daemon.take().expect("daemon").join().expect("joined")
    }
}

#[test]
fn a_handshake_then_a_read_query_returns_an_envelope() {
    let (catalog_path, _) = fixture("read");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1,
        "request_id": "q1",
        "session_id": session,
        "operation": "query",
        "database": "work",
        "statement": "MATCH (n) RETURN count(n) AS total",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "ok", "{reply}");
    assert_eq!(reply["request_id"], "q1", "the request_id must be echoed");

    // Section 5.3: the result is the Engine's envelope, carried verbatim. The daemon states no
    // field of it, so this checks the envelope's own shape rather than a shape invented here.
    let envelope = &reply["result"];
    assert!(envelope["result_version"].is_number(), "{envelope}");
    assert_eq!(envelope["columns"], json!(["total"]));

    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn a_write_commits_and_a_later_read_sees_it() {
    let (catalog_path, _) = fixture("write");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1,
        "request_id": "w1",
        "session_id": session,
        "operation": "query",
        "database": "work",
        "statement": "CREATE (:Service {name: 'checkout'})",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "ok", "{reply}");

    client.send(&json!({
        "server_protocol_version": 1,
        "request_id": "r1",
        "session_id": session,
        "operation": "query",
        "database": "work",
        "statement": "MATCH (s:Service) RETURN s.name AS name",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "ok", "{reply}");
    assert_eq!(reply["result"]["rows"], json!([["checkout"]]), "{reply}");

    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn a_rolled_back_transaction_leaves_nothing_behind() {
    let (catalog_path, _) = fixture("rollback");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "b1",
        "session_id": session, "operation": "begin",
    }));
    assert_eq!(client.receive()["outcome"], "ok");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "w1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "CREATE (:Service {name: 'doomed'})",
    }));
    assert_eq!(client.receive()["outcome"], "ok");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "rb",
        "session_id": session, "operation": "rollback",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "ok", "{reply}");
    assert_eq!(reply["result"]["rolled_back"], true);

    // Outside the region again, and the write is gone.
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "r1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "MATCH (s:Service) RETURN s.name AS name",
    }));
    let reply = client.receive();
    assert_eq!(reply["result"]["rows"], json!([]), "{reply}");

    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn a_committed_transaction_advances_the_generation() {
    let (catalog_path, _) = fixture("commit");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "b1",
        "session_id": session, "operation": "begin",
    }));
    let begun = client.receive();
    let base = begun["result"]["base_generation"]
        .as_u64()
        .expect("a base generation");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "w1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "CREATE (:Service {name: 'kept'})",
    }));
    assert_eq!(client.receive()["outcome"], "ok");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "c1",
        "session_id": session, "operation": "commit",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "ok", "{reply}");
    let committed = reply["result"]["generation"]
        .as_u64()
        .expect("a generation");
    assert!(
        committed > base,
        "a commit that changed something advances the generation: {base} -> {committed}"
    );

    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn dropping_a_connection_inside_a_transaction_rolls_it_back() {
    // Section 6.2: a client that disconnects mid-transaction has not decided to commit, and
    // treating a dropped connection as consent is how a partial write becomes permanent.
    let (catalog_path, database) = fixture("dropped");
    {
        let (mut client, session) = Client::opened(&catalog_path);
        client.send(&json!({
            "server_protocol_version": 1, "request_id": "b1",
            "session_id": session, "operation": "begin",
        }));
        assert_eq!(client.receive()["outcome"], "ok");

        client.send(&json!({
            "server_protocol_version": 1, "request_id": "w1", "session_id": session,
            "operation": "query", "database": "work",
            "statement": "CREATE (:Service {name: 'abandoned'})",
        }));
        assert_eq!(client.receive()["outcome"], "ok");

        // No commit, no rollback: just go away.
        assert_eq!(client.finish(), Ended::ClientClosed);
    }

    // A fresh connection sees nothing, and the file on disk agrees.
    let (mut client, session) = Client::opened(&catalog_path);
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "r1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "MATCH (s:Service) RETURN s.name AS name",
    }));
    let reply = client.receive();
    assert_eq!(
        reply["result"]["rows"],
        json!([]),
        "an abandoned transaction must not have committed: {reply}"
    );
    client.finish();

    let reopened = nostdb_core::storage::Database::open(&database).expect("reopened");
    assert_eq!(
        reopened.generation().get(),
        1,
        "the last valid generation must be untouched"
    );
}

#[test]
fn a_client_speaking_no_common_version_is_refused_and_closed() {
    let (catalog_path, _) = fixture("no-version");
    let mut client = Client::connect(&catalog_path, Limits::default());
    let reply = client.shake(vec![99]);

    assert_eq!(reply["message"], "refused");
    assert_eq!(reply["code"], "SERVER_PROTOCOL_UNSUPPORTED");
    assert_eq!(reply["supported_versions"], json!([1]));
    assert!(
        reply.get("server_protocol_version").is_none(),
        "a refusal names no negotiated version, because there is none"
    );
    assert_eq!(client.finish(), Ended::HandshakeRefused);
}

#[test]
fn a_request_before_the_handshake_is_refused() {
    let (catalog_path, _) = fixture("no-handshake");
    let mut client = Client::connect(&catalog_path, Limits::default());
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "r1", "operation": "status",
    }));
    // The daemon refuses the connection rather than the message: nothing after an absent
    // handshake is interpretable.
    let reply = client.receive();
    assert_eq!(reply["message"], "refused");
    assert_eq!(client.finish(), Ended::HandshakeRefused);
}

#[test]
fn a_second_open_session_is_refused_and_the_connection_survives() {
    let (catalog_path, _) = fixture("second-session");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "open2",
        "operation": "open_session", "database": "work",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    assert_eq!(reply["rule"], "second_session_on_one_connection");

    // The refusal is not fatal: the first session still works, which is the point of refusing
    // rather than replacing it.
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "r1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "MATCH (n) RETURN count(n) AS total",
    }));
    assert_eq!(client.receive()["outcome"], "ok");
    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn a_query_before_any_session_is_refused_as_unknown_session() {
    let (catalog_path, _) = fixture("no-session");
    let mut client = Client::connect(&catalog_path, Limits::default());
    assert_eq!(client.shake(vec![1])["message"], "welcome");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "q1",
        "operation": "query", "database": "work",
        "statement": "MATCH (n) RETURN n",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    assert_eq!(reply["rule"], "unknown_session");
    client.finish();
}

#[test]
fn an_unparseable_statement_reports_the_engine_s_own_code() {
    // Section 5.3 forbids translating an Engine code into one of the daemon's, and forbids adding
    // a code for a failure the Engine already named.
    let (catalog_path, _) = fixture("bad-cypher");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "q1", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "MATCH (n) RETURN",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    let code = reply["diagnostics"][0]["code"]
        .as_str()
        .expect("a diagnostic code");
    assert!(
        code.starts_with("CYPHER_"),
        "the Engine's own code must be forwarded, got {code}"
    );
    assert!(
        reply.get("code").is_none(),
        "the daemon must not add a code of its own for a failure the Engine named"
    );
    client.finish();
}

#[test]
fn a_nested_begin_is_refused_and_the_transaction_survives() {
    let (catalog_path, _) = fixture("nested-begin");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "b1",
        "session_id": session, "operation": "begin",
    }));
    assert_eq!(client.receive()["outcome"], "ok");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "b2",
        "session_id": session, "operation": "begin",
    }));
    assert_eq!(client.receive()["outcome"], "error");

    // Still inside the first transaction, and it still works.
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "s1",
        "session_id": session, "operation": "status",
    }));
    let reply = client.receive();
    assert_eq!(reply["result"]["in_transaction"], true, "{reply}");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "rb",
        "session_id": session, "operation": "rollback",
    }));
    assert_eq!(client.receive()["outcome"], "ok");
    client.finish();
}

#[test]
fn a_commit_with_no_transaction_is_an_error_not_a_refusal() {
    // A well-formed message in the wrong state, which the client can act on. That is what
    // separates an `error` outcome from a section 8 refusal.
    let (catalog_path, _) = fixture("stray-commit");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "c1",
        "session_id": session, "operation": "commit",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    assert!(
        reply.get("rule").is_none(),
        "this is not a protocol refusal"
    );
    client.finish();
}

#[test]
fn a_result_larger_than_the_ceiling_reports_which_limit_stopped_it() {
    // Section 7: a request stopped by a limit reports which limit. "Failed" alone leaves a caller
    // guessing between a bug and a ceiling, and those have opposite fixes.
    let (catalog_path, _) = fixture("row-ceiling");
    let limits = Limits {
        max_result_rows: 1,
        ..Limits::default()
    };
    let mut client = Client::connect(&catalog_path, limits);
    assert_eq!(client.shake(vec![1])["message"], "welcome");
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "open",
        "operation": "open_session", "database": "work",
    }));
    let session = client.receive()["result"]["session_id"]
        .as_str()
        .expect("session")
        .to_owned();

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "w", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "CREATE (:Service {name: 'a'}) CREATE (:Service {name: 'b'})",
    }));
    assert_eq!(client.receive()["outcome"], "ok");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "q", "session_id": session,
        "operation": "query", "database": "work",
        "statement": "MATCH (s:Service) RETURN s.name AS name",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    assert_eq!(reply["limit"], "max_result_rows");
    client.finish();
}

#[test]
fn an_unknown_operation_does_not_end_the_connection() {
    let (catalog_path, _) = fixture("unknown-op");
    let (mut client, session) = Client::opened(&catalog_path);

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "x1", "operation": "vacuum",
    }));
    let reply = client.receive();
    assert_eq!(reply["rule"], "unknown_operation", "{reply}");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "s1",
        "session_id": session, "operation": "status",
    }));
    assert_eq!(client.receive()["outcome"], "ok");
    assert_eq!(client.finish(), Ended::ClientClosed);
}

#[test]
fn shutdown_ends_the_connection_and_says_so() {
    let (catalog_path, _) = fixture("shutdown");
    let (mut client, _) = Client::opened(&catalog_path);
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "down", "operation": "shutdown",
    }));
    assert_eq!(client.receive()["outcome"], "ok");
    assert_eq!(client.finish(), Ended::ShutdownRequested);
}

#[test]
fn two_connections_each_get_their_own_session_and_do_not_see_each_other() {
    // Section 6: requests in different sessions must not observe each other's uncommitted work.
    // Concurrency comes from more connections, which is section 6.1's whole premise.
    let (catalog_path, _) = fixture("two-connections");

    let (mut first, first_session) = Client::opened(&catalog_path);
    let (mut second, second_session) = Client::opened(&catalog_path);
    assert_eq!(
        first_session, second_session,
        "each connection mints its own identifiers, so both are s1"
    );

    first.send(&json!({
        "server_protocol_version": 1, "request_id": "b1",
        "session_id": first_session, "operation": "begin",
    }));
    assert_eq!(first.receive()["outcome"], "ok");
    first.send(&json!({
        "server_protocol_version": 1, "request_id": "w1", "session_id": first_session,
        "operation": "query", "database": "work",
        "statement": "CREATE (:Service {name: 'uncommitted'})",
    }));
    assert_eq!(first.receive()["outcome"], "ok");

    // The second connection reads while the first holds an uncommitted write.
    second.send(&json!({
        "server_protocol_version": 1, "request_id": "r1", "session_id": second_session,
        "operation": "query", "database": "work",
        "statement": "MATCH (s:Service) RETURN s.name AS name",
    }));
    let reply = second.receive();
    assert_eq!(
        reply["result"]["rows"],
        json!([]),
        "one session must not see another's uncommitted work: {reply}"
    );

    first.send(&json!({
        "server_protocol_version": 1, "request_id": "rb",
        "session_id": first_session, "operation": "rollback",
    }));
    assert_eq!(first.receive()["outcome"], "ok");
    first.finish();
    second.finish();
}

#[test]
fn a_session_naming_an_unregistered_database_reports_it_without_ending_the_connection() {
    let (catalog_path, _) = fixture("unregistered");
    let mut client = Client::connect(&catalog_path, Limits::default());
    assert_eq!(client.shake(vec![1])["message"], "welcome");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "o1",
        "operation": "open_session", "database": "absent",
    }));
    let reply = client.receive();
    assert_eq!(reply["outcome"], "error", "{reply}");
    assert!(
        reply["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("absent"),
        "the reply must name what was not found: {reply}"
    );

    // And the connection is still good, so a corrected name works.
    client.send(&json!({
        "server_protocol_version": 1, "request_id": "o2",
        "operation": "open_session", "database": "work",
    }));
    assert_eq!(client.receive()["outcome"], "ok");
    assert_eq!(client.finish(), Ended::ClientClosed);
}

/// A `database` holding a path is refused before any catalog is consulted.
#[test]
fn a_database_path_is_refused_rather_than_resolved() {
    let (catalog_path, database) = fixture("path-database");
    let mut client = Client::connect(&catalog_path, Limits::default());
    assert_eq!(client.shake(vec![1])["message"], "welcome");

    client.send(&json!({
        "server_protocol_version": 1, "request_id": "o1",
        "operation": "open_session", "database": database.display().to_string(),
    }));
    let reply = client.receive();
    assert_eq!(reply["rule"], "database_is_a_path", "{reply}");
    client.finish();
}

/// The client end must be readable and writable for the helper above to be meaningful.
#[test]
fn the_test_harness_really_uses_a_socket() {
    let (mut a, mut b) = UnixStream::pair().expect("pair");
    a.write_all(b"x").expect("written");
    let mut byte = [0_u8; 1];
    b.read_exact(&mut byte).expect("read");
    assert_eq!(&byte, b"x");
}

/// Two clients are served at once, not one after the other.
///
/// This exists because the accept loop was first written to spawn a thread per connection and then
/// join it immediately, which serves connections strictly in turn. That is the opposite of what
/// section 6.1 promises when it says concurrency comes from opening more connections, and nothing
/// else here would have noticed: every other test uses one connection at a time.
#[test]
fn the_accept_loop_serves_two_connections_at_once() {
    use std::sync::mpsc;

    let (catalog_path, _) = fixture("concurrent");

    // A Unix socket path is bounded by SUN_LEN, about 104 bytes, and the scratch directory name
    // used elsewhere here is already longer than that. A socket needs a short path, so this one is
    // built directly under the temporary directory.
    let address = std::env::temp_dir().join(format!("nsdb-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&address);
    let listener = nostdb_server::endpoint::bind(&address).expect("bound");

    let serving = {
        let catalog_path = catalog_path.clone();
        std::thread::spawn(move || {
            let _ =
                nostdb_server::accept_until_shutdown(&listener, &catalog_path, Limits::default());
        })
    };

    // The first client opens a session and holds a transaction open, so its connection is parked
    // inside the transaction region and cannot be serving anything else.
    let mut first = UnixStream::connect(&address).expect("first connected");
    handshake_on(&mut first);
    let first_session = open_on(&mut first);
    send_on(
        &mut first,
        &json!({
            "server_protocol_version": 1, "request_id": "b1",
            "session_id": first_session, "operation": "begin",
        }),
    );
    assert_eq!(receive_on(&mut first)["outcome"], "ok");

    // If connections were served in turn, this second one would never be answered.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut second = UnixStream::connect(&address).expect("second connected");
        handshake_on(&mut second);
        let session = open_on(&mut second);
        send_on(
            &mut second,
            &json!({
                "server_protocol_version": 1, "request_id": "q1", "session_id": session,
                "operation": "query", "database": "work",
                "statement": "MATCH (n) RETURN count(n) AS total",
            }),
        );
        let reply = receive_on(&mut second);
        let _ = tx.send(reply["outcome"] == "ok");
    });

    let answered = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the second connection must be served while the first holds a transaction");
    assert!(answered, "the second connection got an error");

    send_on(
        &mut first,
        &json!({
            "server_protocol_version": 1, "request_id": "rb",
            "session_id": first_session, "operation": "rollback",
        }),
    );
    assert_eq!(receive_on(&mut first)["outcome"], "ok");
    send_on(
        &mut first,
        &json!({
            "server_protocol_version": 1, "request_id": "down", "operation": "shutdown",
        }),
    );
    assert_eq!(receive_on(&mut first)["outcome"], "ok");
    drop(first);
    serving.join().expect("the daemon stopped");
}

fn send_on(stream: &mut UnixStream, value: &Value) {
    let body = serde_json::to_string(value).expect("serialized");
    nostdb_server::frame::write_frame(stream, &body, MAX).expect("written");
}

fn receive_on(stream: &mut UnixStream) -> Value {
    let body = nostdb_server::frame::read_frame(stream, MAX).expect("a reply");
    serde_json::from_str(&body).expect("JSON")
}

fn handshake_on(stream: &mut UnixStream) {
    send_on(
        stream,
        &json!({"message": "hello", "client": "t", "supported_versions": [1]}),
    );
    assert_eq!(receive_on(stream)["message"], "welcome");
}

fn open_on(stream: &mut UnixStream) -> String {
    send_on(
        stream,
        &json!({
            "server_protocol_version": 1, "request_id": "open",
            "operation": "open_session", "database": "work",
        }),
    );
    receive_on(stream)["result"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned()
}
