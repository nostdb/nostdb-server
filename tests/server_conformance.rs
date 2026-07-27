//! The protocol decoder reproduces every refusal `nostdb-spec` declares.
//!
//! The fixtures are read from the superproject at the pinned commit rather than copied here.
//! Each rejected fixture declares the section 8 `rule` it exercises, and this suite requires the
//! decoder to refuse it with exactly that rule rather than merely refusing it: a decoder that
//! reported every malformed message as one rule would satisfy "it was refused" and tell a client
//! nothing.
//!
//! Not every rule is decidable from a document, and the ones that are not are listed rather than
//! skipped. A fixture whose rule appears in neither table fails this suite, so a rule added to
//! the contract cannot quietly go unchecked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nostdb_server::frame::{self, FrameError};
use nostdb_server::message::{self, Rule};

/// Rules this suite exercises through the decoder, and the entry point each one goes through.
///
/// The entry point is named per rule rather than inferred from the document, because two of these
/// are about *where* a message arrived rather than what it contains: a request is perfectly well
/// formed and still refused when it is the first message on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// The opening handshake, including negotiation.
    Handshake,
    /// A request after negotiation.
    Request,
    /// The framing layer, before any JSON is read.
    Frame,
}

fn decidable_rules() -> BTreeMap<&'static str, Entry> {
    BTreeMap::from([
        ("versions_do_not_intersect", Entry::Handshake),
        ("first_message_not_hello", Entry::Handshake),
        ("version_absent_after_handshake", Entry::Request),
        ("body_not_an_object", Entry::Request),
        ("request_id_absent", Entry::Request),
        ("operation_absent", Entry::Request),
        ("unknown_operation", Entry::Request),
        ("database_is_a_path", Entry::Request),
        ("frame_too_large", Entry::Frame),
    ])
}

/// Rules owned elsewhere, with where each is proven.
///
/// These are not gaps in the contract. They are refusals that depend on state a document does not
/// carry, so a document-driven suite cannot decide them, and saying so is better than a fixture
/// that appears covered.
fn rules_proven_elsewhere() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            "unknown_session",
            "needs a session registry, which arrives with sessions",
        ),
        (
            "peer_is_another_user",
            "held structurally by the endpoint's directory mode, proven in the endpoint tests",
        ),
    ])
}

fn fixture_root() -> Option<PathBuf> {
    let raw = std::env::var("NOSTDB_SPEC_FIXTURES").ok()?;
    let directory = PathBuf::from(raw).join("server");
    directory.is_dir().then_some(directory)
}

fn expectations(path: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(path.with_extension("expected")).unwrap_or_else(|error| {
        panic!("{} has no readable .expected file: {error}", path.display())
    });
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    map
}

fn documents(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
}

/// Refuses `body` through the entry point the rule names, returning the rule it was refused with.
fn refusal_rule(entry: Entry, body: &str, document: &serde_json::Value) -> Option<Rule> {
    match entry {
        Entry::Handshake => match message::decode_hello(body) {
            Err(refusal) => Some(refusal.rule()),
            Ok(hello) => message::negotiate(&hello).err().map(|r| r.rule()),
        },
        Entry::Request => message::decode_request(body, &BTreeSet::new())
            .err()
            .map(|refusal| refusal.rule()),
        Entry::Frame => {
            // The fixture records the length a peer declared. A frame carrying only that prefix
            // and no body is exactly the hostile case: a reader that sized its buffer before
            // validating would allocate it.
            let declared = document
                .get("declared_frame_bytes")
                .and_then(serde_json::Value::as_u64)
                .expect("a frame fixture states declared_frame_bytes");
            let declared = u32::try_from(declared).expect("a declared length fits in u32");
            let mut cursor = std::io::Cursor::new(declared.to_be_bytes().to_vec());
            match frame::read_frame(&mut cursor, frame::MINIMUM_MAXIMUM_FRAME_BYTES) {
                Err(FrameError::TooLarge { .. }) => Some(Rule::FrameTooLarge),
                _ => None,
            }
        }
    }
}

#[test]
fn every_rejected_fixture_is_refused_with_the_rule_it_declares() {
    let Some(root) = fixture_root() else {
        println!("server conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };
    let decidable = decidable_rules();
    let elsewhere = rules_proven_elsewhere();

    let paths = documents(&root.join("invalid"));
    assert!(!paths.is_empty(), "no rejected fixtures were found");

    let mut checked = 0_usize;
    let mut deferred = Vec::new();

    for path in &paths {
        let expected = expectations(path);
        let declared_rule = expected
            .get("rule")
            .unwrap_or_else(|| panic!("{} declares no rule", path.display()));

        if let Some(reason) = elsewhere.get(declared_rule.as_str()) {
            deferred.push(format!("{declared_rule} ({reason})"));
            continue;
        }

        let entry = decidable.get(declared_rule.as_str()).unwrap_or_else(|| {
            panic!(
                "{} declares rule {declared_rule}, which this suite neither decides nor lists as proven elsewhere",
                path.display()
            )
        });

        let body = std::fs::read_to_string(path).expect("fixture is UTF-8");
        let document: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

        let refused = refusal_rule(*entry, &body, &document)
            .unwrap_or_else(|| panic!("{} was accepted, and must be refused", path.display()));

        assert_eq!(
            refused.as_str(),
            declared_rule.as_str(),
            "{} declares {declared_rule} and was refused with {refused}",
            path.display()
        );

        // The code, where the contract assigns one. Section 8 assigns one to the version refusal
        // alone, so this also proves the others carry none.
        match expected.get("code") {
            Some(code) => assert_eq!(
                refused.code().map(|c| c.as_str()),
                Some(code.as_str()),
                "{} declares code {code}",
                path.display()
            ),
            None => assert_eq!(
                refused.code(),
                None,
                "{} declares no code, so the refusal must carry none",
                path.display()
            ),
        }
        checked += 1;
    }

    // Reported rather than left implicit: a suite that covered nine of eleven rules and said
    // nothing would read as covering all of them.
    for entry in &deferred {
        println!("server conformance: deferred, {entry}");
    }
    println!("server conformance: {checked} refusal rules verified");
}

#[test]
fn every_accepted_request_fixture_is_accepted() {
    let Some(root) = fixture_root() else {
        println!("server conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };

    let mut requests = 0_usize;
    let mut handshakes = 0_usize;
    for path in documents(&root.join("valid")) {
        let body = std::fs::read_to_string(&path).expect("fixture is UTF-8");
        let document: serde_json::Value = serde_json::from_str(&body).expect("fixture is JSON");

        match document.get("message").and_then(serde_json::Value::as_str) {
            Some("hello") => {
                let hello = message::decode_hello(&body)
                    .unwrap_or_else(|r| panic!("{} was refused: {r}", path.display()));
                message::negotiate(&hello)
                    .unwrap_or_else(|r| panic!("{} did not negotiate: {r}", path.display()));
                handshakes += 1;
            }
            // `welcome`, `refused`, and a response are messages the daemon *sends*. The decoder
            // has no entry point for them by design: a daemon that could decode its own replies
            // would be a client, and this crate is not one.
            Some(_) => {}
            None if document.get("operation").is_some() => {
                message::decode_request(&body, &BTreeSet::new())
                    .unwrap_or_else(|r| panic!("{} was refused: {r}", path.display()));
                requests += 1;
            }
            None => {}
        }
    }
    assert!(requests > 0, "no accepted request fixture was exercised");
    assert!(
        handshakes > 0,
        "no accepted handshake fixture was exercised"
    );
    println!("server conformance: {handshakes} handshakes and {requests} requests verified");
}

/// The daemon's own replies match the fixtures published for them.
///
/// This is the direction the decoder cannot check: the fixtures for `welcome` and `refused` say
/// what a daemon must send, and nothing else here reads them.
#[test]
fn the_daemon_replies_match_the_published_shapes() {
    let Some(root) = fixture_root() else {
        println!("server conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };

    let welcome_fixture: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("valid/welcome.json")).expect("welcome fixture"),
    )
    .expect("JSON");
    let produced = message::welcome(1, "/home/dana/.nostdb/run/nostdb.sock");
    assert_eq!(
        produced, welcome_fixture,
        "the welcome this daemon sends differs from the published one"
    );

    let refused_fixture: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("valid/refused_version.json")).expect("refusal fixture"),
    )
    .expect("JSON");
    assert_eq!(
        message::refused(),
        refused_fixture,
        "the refusal this daemon sends differs from the published one"
    );
    println!("server conformance: 2 published replies verified");
}
