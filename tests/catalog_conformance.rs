//! The catalog reader reproduces every outcome `nostdb-spec` declares.
//!
//! The fixtures are read from the superproject at the pinned commit rather than copied here, so
//! there is one published suite and not a vendored second one that drifts. The workspace
//! verifier sets `NOSTDB_SPEC_FIXTURES`; without it these tests skip, so a standalone clone of
//! this repository still builds and tests.
//!
//! A skip proves nothing, which is why the workspace verifier requires the confirmation line
//! each test prints.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nostdb_server::catalog::Catalog;

fn fixture_root() -> Option<PathBuf> {
    let raw = std::env::var("NOSTDB_SPEC_FIXTURES").ok()?;
    let directory = PathBuf::from(raw).join("catalog");
    directory.is_dir().then_some(directory)
}

/// The `key = value` lines beside a fixture.
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

#[test]
fn every_accepted_fixture_is_read() {
    let Some(root) = fixture_root() else {
        println!("catalog conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };
    let paths = documents(&root.join("valid"));
    assert!(!paths.is_empty(), "no accepted fixtures were found");

    for path in &paths {
        let expected = expectations(path);
        assert_eq!(
            expected.get("outcome").map(String::as_str),
            Some("accept"),
            "{} must declare outcome = accept",
            path.display()
        );
        let text = std::fs::read_to_string(path).expect("fixture is UTF-8");
        Catalog::parse(&text)
            .unwrap_or_else(|rejection| panic!("{} was refused: {rejection}", path.display()));
    }
    println!(
        "catalog conformance: {} accepted fixtures verified",
        paths.len()
    );
}

#[test]
fn every_rejected_fixture_is_refused_with_the_code_it_declares() {
    let Some(root) = fixture_root() else {
        println!("catalog conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };
    let paths = documents(&root.join("invalid"));
    assert!(!paths.is_empty(), "no rejected fixtures were found");

    for path in &paths {
        let expected = expectations(path);
        assert_eq!(
            expected.get("outcome").map(String::as_str),
            Some("reject"),
            "{} must declare outcome = reject",
            path.display()
        );
        let declared = expected
            .get("code")
            .unwrap_or_else(|| panic!("{} declares no code", path.display()));

        let text = std::fs::read_to_string(path).expect("fixture is UTF-8");
        let rejection =
            Catalog::parse(&text).expect_err(&format!("{} must be refused", path.display()));

        // The declared code, not merely the refusal. An unsupported version and a malformed
        // document are different failures for a caller, and a test that only asserted "refused"
        // would pass while reporting either one for the other.
        assert_eq!(
            rejection.code().as_str(),
            declared.as_str(),
            "{} declares {declared} and was refused with {}",
            path.display(),
            rejection.code()
        );
        assert!(
            !rejection.problems().is_empty(),
            "{} was refused with no problem stated",
            path.display()
        );
    }
    println!(
        "catalog conformance: {} rejected fixtures verified",
        paths.len()
    );
}

/// A round trip through the accepted fixtures preserves what this build does not understand.
///
/// The preservation rule in the contract's section 7 is the one that makes an older build safe
/// to run against a newer catalog. It cannot be checked by parsing alone, because dropping an
/// unknown member parses perfectly well.
#[test]
fn an_accepted_fixture_round_trips_without_losing_an_unknown_member() {
    let Some(root) = fixture_root() else {
        println!("catalog conformance: skipped, NOSTDB_SPEC_FIXTURES is unset");
        return;
    };

    let mut checked = 0_usize;
    for path in documents(&root.join("valid")) {
        let text = std::fs::read_to_string(&path).expect("fixture is UTF-8");
        let original: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
        let catalog = Catalog::parse(&text).expect("accepted");
        let rewritten = catalog.to_document();

        for (key, value) in original.as_object().expect("an object") {
            if key == "databases" {
                continue;
            }
            assert_eq!(
                rewritten.get(key),
                Some(value),
                "{} lost top-level member {key} on rewrite",
                path.display()
            );
        }

        for (name, entry) in original["databases"].as_object().expect("an object") {
            for (key, value) in entry.as_object().expect("an entry object") {
                assert_eq!(
                    rewritten["databases"][name].get(key),
                    Some(value),
                    "{} lost {name}.{key} on rewrite",
                    path.display()
                );
            }
        }
        checked += 1;
    }
    println!("catalog conformance: {checked} round trips verified");
}
