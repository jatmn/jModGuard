//! GHSA-8m95-fffc-h4c5 / CVE-2025-47736: invalid UTF-8 must not panic.

use fallible_iterator::FallibleIterator;
use libsql_sqlite3_parser::lexer::sql::Parser;

#[test]
fn cargo_lock_has_no_vulnerable_libsql_sqlite3_parser() {
    let lock = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"));
    let versions = package_versions(lock, "libsql-sqlite3-parser");
    assert!(
        !versions.is_empty(),
        "Cargo.lock must still pin libsql-sqlite3-parser"
    );
    for version in &versions {
        assert!(
            version_is_patched(version),
            "vulnerable libsql-sqlite3-parser {version} still in Cargo.lock (GHSA-8m95-fffc-h4c5)"
        );
    }
}

#[test]
fn invalid_utf8_debug_does_not_panic() {
    // Advisory PoC: `SELECT ? ` followed by overlong NUL bytes, then Debug.
    let mut sql = b"SELECT ? ".to_vec();
    sql.extend_from_slice(&[0xC0, 0x80]);
    let mut parser = Parser::new(&sql);
    let cmd = parser
        .next()
        .expect("parser error")
        .expect("expected a statement");
    let rendered = format!("{cmd:?}");
    assert!(
        !rendered.is_empty(),
        "Debug of a parsed command with invalid UTF-8 must succeed"
    );
}

fn package_versions<'a>(lock: &'a str, package: &str) -> Vec<&'a str> {
    let mut versions = Vec::new();
    let mut matching = false;
    for line in lock.lines() {
        if line == "[[package]]" {
            matching = false;
        } else if line == format!("name = \"{package}\"") {
            matching = true;
        } else if matching && let Some(version) = line.strip_prefix("version = \"") {
            versions.push(version.trim_end_matches('"'));
            matching = false;
        }
    }
    versions
}

fn version_is_patched(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(major) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    let Some(patch) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    !matches!((major, minor, patch), (0, 0..=12, _) | (0, 13, 0))
}
