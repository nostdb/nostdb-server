//! The diagnostic codes this repository owns.
//!
//! `nostdb-spec` publishes these four and records `nostdb-server` as their owner, because the
//! daemon raises them and the Engine never can. The workspace verifier compares the registry
//! against this file, so a code added here without being published there fails, and a code
//! published there that never appears here fails too.
//!
//! Every other code the daemon reports comes from the Engine and is forwarded unchanged. The
//! protocol contract's section 5.3 forbids translating one, so there is no case for it here.

use std::fmt;

/// A diagnostic code this crate raises.
///
/// The string forms are stable public identifiers. Renaming one is a breaking change to every
/// client that matches on it, which is why the workspace verifier holds them against the
/// published registry rather than trusting review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// A catalog breaks a rule decidable by reading it.
    ///
    /// A stale entry target is deliberately not one of those rules. See
    /// [`crate::catalog`] and the catalog contract's section 1.3.
    CatalogInvalid,
    /// The `catalog_version` is not one this build reads.
    CatalogVersionUnsupported,
    /// A start request found a healthy daemon.
    ///
    /// Reported as the outcome of a start rather than as a failure: starting something already
    /// started is what the caller asked for.
    ServerAlreadyRunning,
    /// The client and the daemon support no common `server_protocol_version`.
    ServerProtocolUnsupported,
}

impl Code {
    /// The stable identifier a client matches on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CatalogInvalid => "CATALOG_INVALID",
            Self::CatalogVersionUnsupported => "CATALOG_VERSION_UNSUPPORTED",
            Self::ServerAlreadyRunning => "SERVER_ALREADY_RUNNING",
            Self::ServerProtocolUnsupported => "SERVER_PROTOCOL_UNSUPPORTED",
        }
    }

    /// Whether this code reports a failure.
    ///
    /// `SERVER_ALREADY_RUNNING` is the one that does not, and it is the reason this method
    /// exists rather than callers assuming every code is an error.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        !matches!(self, Self::ServerAlreadyRunning)
    }
}

impl fmt::Display for Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn every_code_has_a_distinct_stable_form() {
        let all = [
            Code::CatalogInvalid,
            Code::CatalogVersionUnsupported,
            Code::ServerAlreadyRunning,
            Code::ServerProtocolUnsupported,
        ];
        let mut forms: Vec<&str> = all.iter().map(|code| code.as_str()).collect();
        forms.sort_unstable();
        let count = forms.len();
        forms.dedup();
        assert_eq!(forms.len(), count, "two codes share one identifier");
    }

    #[test]
    fn only_the_already_running_outcome_is_not_a_failure() {
        assert!(!Code::ServerAlreadyRunning.is_failure());
        assert!(Code::CatalogInvalid.is_failure());
        assert!(Code::CatalogVersionUnsupported.is_failure());
        assert!(Code::ServerProtocolUnsupported.is_failure());
    }
}
