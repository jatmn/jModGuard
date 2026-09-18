//! GHSA-82j2-j2ch-gfr8: malformed CRL BIT STRING must not panic.

#[test]
fn cargo_lock_has_no_vulnerable_rustls_webpki() {
    let lock = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"));
    let versions = package_versions(lock, "rustls-webpki");
    assert!(
        !versions.is_empty(),
        "Cargo.lock must still pin rustls-webpki"
    );
    for version in &versions {
        assert!(
            !version.starts_with("0.102."),
            "vulnerable rustls-webpki {version} still in Cargo.lock (GHSA-82j2-j2ch-gfr8)"
        );
        assert!(
            version_is_patched(version),
            "unpatched rustls-webpki {version} still in Cargo.lock (GHSA-82j2-j2ch-gfr8)"
        );
    }
}

#[test]
fn malformed_crl_bit_string_returns_error_instead_of_panicking() {
    // Advisory PoC: issuingDistributionPoint onlySomeReasons BIT STRING [0x00].
    let crl: &[u8] = &[
        0x30, 0x65, 0x30, 0x50, 0x02, 0x01, 0x01, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86,
        0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x0c, 0x31, 0x0a, 0x30, 0x08, 0x06, 0x03,
        0x55, 0x04, 0x03, 0x13, 0x01, 0x41, 0x17, 0x0d, 0x32, 0x30, 0x30, 0x31, 0x30, 0x31, 0x30,
        0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x17, 0x0d, 0x32, 0x31, 0x30, 0x31, 0x30, 0x31, 0x30,
        0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0xa0, 0x10, 0x30, 0x0e, 0x30, 0x0c, 0x06, 0x03, 0x55,
        0x1d, 0x1c, 0x04, 0x05, 0x30, 0x03, 0x83, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86,
        0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x03, 0x02, 0x00, 0x00,
    ];
    assert!(webpki::BorrowedCertRevocationList::from_der(crl).is_err());
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
    if let Some(rest) = version.strip_prefix("0.103.") {
        rest.parse::<u32>().is_ok_and(|patch| patch >= 13)
    } else if let Some(rest) = version.strip_prefix("0.104.0-alpha.") {
        rest.parse::<u32>().is_ok_and(|n| n >= 7)
    } else {
        !version.starts_with("0.10")
    }
}
