//! Message decoding and version negotiation, as `server_protocol_version` 1 sections 4, 5, and
//! 8 define them.
//!
//! Every refusal here names the section 8 row it comes from, as a [`Rule`]. That is what lets
//! `tests/server_conformance.rs` check this module against the published fixtures by rule rather
//! than by prose: a fixture declares `rule = database_is_a_path`, and the decoder must refuse it
//! with exactly that.
//!
//! Only two refusals carry a diagnostic code, because section 8 assigns one to the version
//! refusal alone. The rest are malformed rather than unauthorized, and a code is a contract with
//! a caller: a peer that cannot frame a message is not yet a caller.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Map, Value};

use crate::diagnostic::Code;

/// The versions of the local protocol this build speaks.
pub const SUPPORTED_VERSIONS: &[u64] = &[1];

/// A section 8 refusal row.
///
/// The string forms match the `rule` a published fixture declares, so the conformance suite
/// compares them directly instead of matching on message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// The client and daemon share no version.
    VersionsDoNotIntersect,
    /// A message after the handshake states no version.
    VersionAbsentAfterHandshake,
    /// The first message on a connection was not `hello`.
    FirstMessageNotHello,
    /// A frame declared more bytes than the maximum allows.
    FrameTooLarge,
    /// The frame body was not a JSON object.
    BodyNotAnObject,
    /// A request carried no `request_id`, or repeated an outstanding one.
    RequestIdAbsent,
    /// A request named no operation.
    OperationAbsent,
    /// A request named an operation this contract does not publish.
    UnknownOperation,
    /// `database` held a filesystem path rather than a catalog name.
    DatabaseIsAPath,
    /// A request named a session that does not exist.
    UnknownSession,
    /// The peer belongs to another operating-system user.
    PeerIsAnotherUser,
}

impl Rule {
    /// The identifier a published fixture declares.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VersionsDoNotIntersect => "versions_do_not_intersect",
            Self::VersionAbsentAfterHandshake => "version_absent_after_handshake",
            Self::FirstMessageNotHello => "first_message_not_hello",
            Self::FrameTooLarge => "frame_too_large",
            Self::BodyNotAnObject => "body_not_an_object",
            Self::RequestIdAbsent => "request_id_absent",
            Self::OperationAbsent => "operation_absent",
            Self::UnknownOperation => "unknown_operation",
            Self::DatabaseIsAPath => "database_is_a_path",
            Self::UnknownSession => "unknown_session",
            Self::PeerIsAnotherUser => "peer_is_another_user",
        }
    }

    /// The diagnostic code this refusal carries, if the contract assigns one.
    ///
    /// Section 8 assigns a code to the version refusal alone.
    #[must_use]
    pub const fn code(self) -> Option<Code> {
        match self {
            Self::VersionsDoNotIntersect => Some(Code::ServerProtocolUnsupported),
            _ => None,
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A refused message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    rule: Rule,
    detail: String,
}

impl Refusal {
    /// The section 8 row this refusal comes from.
    #[must_use]
    pub const fn rule(&self) -> Rule {
        self.rule
    }

    /// The diagnostic code, if the contract assigns one to this rule.
    #[must_use]
    pub const fn code(&self) -> Option<Code> {
        self.rule.code()
    }

    /// Why the message was refused.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    fn new(rule: Rule, detail: impl Into<String>) -> Self {
        Self {
            rule,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.rule, self.detail)
    }
}

impl std::error::Error for Refusal {}

/// An operation section 5.2 publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Starts a session.
    OpenSession,
    /// Ends a session, rolling back an open transaction.
    CloseSession,
    /// Runs one statement against the named database.
    Query,
    /// Begins an explicit transaction within a session.
    Begin,
    /// Commits the session's transaction.
    Commit,
    /// Rolls back the session's transaction.
    Rollback,
    /// Reports the daemon's endpoint, uptime, and session count.
    Status,
    /// Stops the daemon after ending its sessions.
    Shutdown,
}

impl Operation {
    /// Parses an operation name, refusing an unpublished one rather than guessing.
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "open_session" => Self::OpenSession,
            "close_session" => Self::CloseSession,
            "query" => Self::Query,
            "begin" => Self::Begin,
            "commit" => Self::Commit,
            "rollback" => Self::Rollback,
            "status" => Self::Status,
            "shutdown" => Self::Shutdown,
            _ => return None,
        })
    }

    /// The name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenSession => "open_session",
            Self::CloseSession => "close_session",
            Self::Query => "query",
            Self::Begin => "begin",
            Self::Commit => "commit",
            Self::Rollback => "rollback",
            Self::Status => "status",
            Self::Shutdown => "shutdown",
        }
    }
}

/// A client's opening message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The versions the client speaks.
    pub supported_versions: Vec<u64>,
    /// What the client calls itself. Advisory, and never trusted for a decision.
    pub client: Option<String>,
}

/// A decoded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The negotiated version this message states.
    pub server_protocol_version: u64,
    /// The client's opaque correlation token, echoed in the response.
    pub request_id: String,
    /// The session this request belongs to, when it needs one.
    pub session_id: Option<String>,
    /// What to do.
    pub operation: Operation,
    /// The catalog name, without the sigil.
    pub database: Option<String>,
    /// The statement, for a query.
    pub statement: Option<String>,
}

/// Decodes the opening `hello`.
///
/// # Errors
///
/// Returns [`Rule::BodyNotAnObject`] when the body is not a JSON object, and
/// [`Rule::FirstMessageNotHello`] when it is some other message. Note that this does **not**
/// negotiate: see [`negotiate`].
pub fn decode_hello(body: &str) -> Result<Hello, Refusal> {
    let object = object_of(body)?;

    match object.get("message").and_then(Value::as_str) {
        Some("hello") => {}
        Some(other) => {
            return Err(Refusal::new(
                Rule::FirstMessageNotHello,
                format!("the first message on a connection is hello, not {other}"),
            ));
        }
        None => {
            return Err(Refusal::new(
                Rule::FirstMessageNotHello,
                "the first message on a connection is hello, and this names no message",
            ));
        }
    }

    // A `hello` states no `server_protocol_version` of its own, and one that did would be a
    // client that already knew the answer it is asking for. It is not refused for stating one,
    // because that is a client being redundant rather than unintelligible; the value is ignored.
    let supported_versions = object
        .get("supported_versions")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_u64).collect::<Vec<_>>())
        .unwrap_or_default();

    Ok(Hello {
        supported_versions,
        client: object
            .get("client")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Chooses the highest version both sides speak.
///
/// # Errors
///
/// Returns [`Rule::VersionsDoNotIntersect`], which is the one refusal carrying
/// [`Code::ServerProtocolUnsupported`].
pub fn negotiate(hello: &Hello) -> Result<u64, Refusal> {
    let ours: BTreeSet<u64> = SUPPORTED_VERSIONS.iter().copied().collect();
    hello
        .supported_versions
        .iter()
        .copied()
        .filter(|version| ours.contains(version))
        .max()
        .ok_or_else(|| {
            Refusal::new(
                Rule::VersionsDoNotIntersect,
                format!(
                    "the client offers {:?} and this build speaks {SUPPORTED_VERSIONS:?}",
                    hello.supported_versions
                ),
            )
        })
}

/// Decodes a request sent after negotiation.
///
/// `outstanding` is the set of `request_id` values already in flight on this connection. A repeat
/// is refused: responses may arrive in any order, so a client that reused an identifier could not
/// match a reply to the request it answers.
///
/// # Errors
///
/// Returns the section 8 rule the message breaks. Every check here is decidable from the message
/// and the connection's own state; whether a session or a catalog name exists is not, and is
/// decided by the caller that owns those.
pub fn decode_request(body: &str, outstanding: &BTreeSet<String>) -> Result<Request, Refusal> {
    let object = object_of(body)?;

    // The handshake has happened, so a message that states no version might have come from a
    // differently versioned process and must not be read as though it had.
    let Some(version) = object
        .get("server_protocol_version")
        .and_then(Value::as_u64)
    else {
        return Err(Refusal::new(
            Rule::VersionAbsentAfterHandshake,
            "every message after the handshake states the negotiated server_protocol_version",
        ));
    };
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(Refusal::new(
            Rule::VersionsDoNotIntersect,
            format!("this build does not speak server_protocol_version {version}"),
        ));
    }

    // A `hello` arriving after the handshake is the handshake happening twice, which section 9
    // rules out by making the version unchangeable within a connection.
    if object.get("message").and_then(Value::as_str) == Some("hello") {
        return Err(Refusal::new(
            Rule::FirstMessageNotHello,
            "the handshake happens once per connection, and the version cannot change within one",
        ));
    }

    let request_id = match object.get("request_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => {
            return Err(Refusal::new(
                Rule::RequestIdAbsent,
                "a request carries a non-empty request_id, because responses may arrive in any order",
            ));
        }
    };
    if outstanding.contains(&request_id) {
        return Err(Refusal::new(
            Rule::RequestIdAbsent,
            format!("request_id {request_id} is already outstanding on this connection"),
        ));
    }

    let operation = match object.get("operation").and_then(Value::as_str) {
        None => {
            return Err(Refusal::new(
                Rule::OperationAbsent,
                "a request names an operation, and there is no default to fall back to",
            ));
        }
        Some(name) => Operation::parse(name).ok_or_else(|| {
            Refusal::new(
                Rule::UnknownOperation,
                format!("{name} is not an operation this contract publishes"),
            )
        })?,
    };

    let database = match object.get("database").and_then(Value::as_str) {
        None => None,
        Some(name) => {
            if let Some(problem) = path_shaped(name) {
                return Err(Refusal::new(Rule::DatabaseIsAPath, problem));
            }
            Some(name.to_owned())
        }
    };

    Ok(Request {
        server_protocol_version: version,
        request_id,
        session_id: object
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        operation,
        database,
        statement: object
            .get("statement")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// The `welcome` a daemon answers a successful handshake with.
#[must_use]
pub fn welcome(version: u64, endpoint: &str) -> Value {
    let mut object = Map::new();
    object.insert("message".to_owned(), Value::String("welcome".to_owned()));
    object.insert(
        "server_protocol_version".to_owned(),
        Value::Number(version.into()),
    );
    object.insert("endpoint".to_owned(), Value::String(endpoint.to_owned()));
    Value::Object(object)
}

/// The `refused` a daemon answers a failed negotiation with.
///
/// It states no `server_protocol_version`, because there is none: the two sides have just
/// established that they share none, and naming one would be a claim about a language neither
/// agreed to speak. It states the supported set instead, which is the actionable part.
#[must_use]
pub fn refused() -> Value {
    let mut object = Map::new();
    object.insert("message".to_owned(), Value::String("refused".to_owned()));
    object.insert(
        "code".to_owned(),
        Value::String(Code::ServerProtocolUnsupported.as_str().to_owned()),
    );
    object.insert(
        "supported_versions".to_owned(),
        Value::Array(
            SUPPORTED_VERSIONS
                .iter()
                .map(|version| Value::Number((*version).into()))
                .collect(),
        ),
    );
    Value::Object(object)
}

fn object_of(body: &str) -> Result<Map<String, Value>, Refusal> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        Refusal::new(
            Rule::BodyNotAnObject,
            format!("the frame body is not readable JSON: {error}"),
        )
    })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(Refusal::new(
            Rule::BodyNotAnObject,
            "the frame body is a JSON object",
        )),
    }
}

/// Whether a `database` value is a path rather than a catalog name.
///
/// The daemon is not a second route to a file. A path belongs to Embedded Mode, which needs no
/// daemon at all, and accepting one here would create two routes to a file with different rules.
fn path_shaped(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("database is a catalog name, and this is empty".to_owned());
    }
    if name.starts_with('@') {
        return Some(format!(
            "{name}: the @ sigil belongs to the command line, and database carries the bare name"
        ));
    }
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Some(format!(
            "{name} is a path, and the daemon resolves catalog names rather than paths"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        Hello, Operation, Rule, decode_hello, decode_request, negotiate, refused, welcome,
    };
    use crate::diagnostic::Code;
    use std::collections::BTreeSet;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn a_hello_offering_a_supported_version_negotiates_it() {
        let hello =
            decode_hello(r#"{"message":"hello","client":"nostdb-cli","supported_versions":[1]}"#)
                .expect("decoded");
        assert_eq!(negotiate(&hello).expect("negotiated"), 1);
    }

    #[test]
    fn a_hello_offering_nothing_in_common_is_the_one_refusal_with_a_code() {
        let hello = Hello {
            supported_versions: vec![99],
            client: None,
        };
        let refusal = negotiate(&hello).expect_err("refused");
        assert_eq!(refusal.rule(), Rule::VersionsDoNotIntersect);
        assert_eq!(refusal.code(), Some(Code::ServerProtocolUnsupported));
    }

    #[test]
    fn every_other_refusal_carries_no_code() {
        // Section 8 assigns a code to the version refusal alone, because a code is a contract
        // with a caller and a peer that cannot frame a message is not yet a caller.
        for rule in [
            Rule::VersionAbsentAfterHandshake,
            Rule::FirstMessageNotHello,
            Rule::FrameTooLarge,
            Rule::BodyNotAnObject,
            Rule::RequestIdAbsent,
            Rule::OperationAbsent,
            Rule::UnknownOperation,
            Rule::DatabaseIsAPath,
            Rule::UnknownSession,
            Rule::PeerIsAnotherUser,
        ] {
            assert_eq!(rule.code(), None, "{rule} must carry no code");
        }
    }

    #[test]
    fn a_first_message_that_is_not_hello_is_refused() {
        let refusal = decode_hello(r#"{"server_protocol_version":1,"operation":"status"}"#)
            .expect_err("refused");
        assert_eq!(refusal.rule(), Rule::FirstMessageNotHello);
    }

    #[test]
    fn a_body_that_is_not_an_object_is_refused() {
        assert_eq!(
            decode_hello("[]").expect_err("refused").rule(),
            Rule::BodyNotAnObject
        );
        assert_eq!(
            decode_request("[]", &none()).expect_err("refused").rule(),
            Rule::BodyNotAnObject
        );
    }

    #[test]
    fn a_request_without_a_version_is_refused() {
        let refusal = decode_request(r#"{"request_id":"r1","operation":"status"}"#, &none())
            .expect_err("refused");
        assert_eq!(refusal.rule(), Rule::VersionAbsentAfterHandshake);
    }

    #[test]
    fn a_request_without_a_request_id_is_refused() {
        let refusal = decode_request(
            r#"{"server_protocol_version":1,"operation":"status"}"#,
            &none(),
        )
        .expect_err("refused");
        assert_eq!(refusal.rule(), Rule::RequestIdAbsent);
    }

    #[test]
    fn a_repeated_outstanding_request_id_is_refused() {
        // Responses may arrive in any order, so a reused identifier makes a reply unmatchable.
        let mut outstanding = BTreeSet::new();
        outstanding.insert("r1".to_owned());
        let refusal = decode_request(
            r#"{"server_protocol_version":1,"request_id":"r1","operation":"status"}"#,
            &outstanding,
        )
        .expect_err("refused");
        assert_eq!(refusal.rule(), Rule::RequestIdAbsent);
    }

    #[test]
    fn the_same_request_id_is_accepted_once_it_is_no_longer_outstanding() {
        let request = decode_request(
            r#"{"server_protocol_version":1,"request_id":"r1","operation":"status"}"#,
            &none(),
        )
        .expect("accepted");
        assert_eq!(request.request_id, "r1");
    }

    #[test]
    fn an_absent_and_an_unknown_operation_are_different_refusals() {
        assert_eq!(
            decode_request(
                r#"{"server_protocol_version":1,"request_id":"r1"}"#,
                &none()
            )
            .expect_err("refused")
            .rule(),
            Rule::OperationAbsent
        );
        assert_eq!(
            decode_request(
                r#"{"server_protocol_version":1,"request_id":"r1","operation":"vacuum"}"#,
                &none()
            )
            .expect_err("refused")
            .rule(),
            Rule::UnknownOperation
        );
    }

    #[test]
    fn a_database_holding_a_path_is_refused_in_every_spelling() {
        for value in [
            "./project/.nostdb/root.nostdb",
            "/srv/db.nostdb",
            "work/main",
            "@work",
            "..",
            "",
        ] {
            let body = format!(
                r#"{{"server_protocol_version":1,"request_id":"r1","operation":"query","database":"{value}","statement":"MATCH (n) RETURN n"}}"#
            );
            let refusal = decode_request(&body, &none()).unwrap_err_or_else_message(value);
            assert_eq!(
                refusal.rule(),
                Rule::DatabaseIsAPath,
                "{value} must be refused as a path"
            );
        }
    }

    #[test]
    fn a_bare_catalog_name_is_accepted() {
        let request = decode_request(
            r#"{"server_protocol_version":1,"request_id":"r1","session_id":"s1","operation":"query","database":"work","statement":"MATCH (n) RETURN n"}"#,
            &none(),
        )
        .expect("accepted");
        assert_eq!(request.database.as_deref(), Some("work"));
        assert_eq!(request.operation, Operation::Query);
        assert_eq!(request.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn a_second_hello_after_the_handshake_is_refused() {
        // Section 9 makes the version unchangeable within a connection, so renegotiating is not
        // a thing a client may do part way through.
        let refusal = decode_request(
            r#"{"server_protocol_version":1,"request_id":"r2","message":"hello","operation":"status"}"#,
            &none(),
        )
        .expect_err("refused");
        assert_eq!(refusal.rule(), Rule::FirstMessageNotHello);
    }

    #[test]
    fn a_welcome_states_the_negotiated_version_and_a_refusal_states_none() {
        let welcome = welcome(1, "/home/dana/.nostdb/run/nostdb.sock");
        assert_eq!(welcome["server_protocol_version"], 1);
        assert_eq!(welcome["message"], "welcome");

        let refused = refused();
        assert!(
            refused.get("server_protocol_version").is_none(),
            "a refusal names no negotiated version, because there is none"
        );
        assert_eq!(refused["code"], "SERVER_PROTOCOL_UNSUPPORTED");
        assert_eq!(refused["supported_versions"], serde_json::json!([1]));
    }

    /// A small helper so the loop above reports which spelling failed.
    trait UnwrapErrOrElseMessage {
        fn unwrap_err_or_else_message(self, value: &str) -> super::Refusal;
    }

    impl UnwrapErrOrElseMessage for Result<super::Request, super::Refusal> {
        fn unwrap_err_or_else_message(self, value: &str) -> super::Refusal {
            match self {
                Err(refusal) => refusal,
                Ok(request) => {
                    panic!("{value} was accepted as database {:?}", request.database)
                }
            }
        }
    }
}
