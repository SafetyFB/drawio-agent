//! Tests for the checksum verification logic.

use drawio_agent_renderer::checksum::{expected_for, verify_checksum, CHECKSUMS};

#[test]
fn all_four_platforms_have_pinned_hashes() {
    // Verify the table is fully populated (no empty entries).
    assert_eq!(CHECKSUMS.len(), 4);
    for (platform, hash) in CHECKSUMS {
        assert!(!hash.is_empty(), "{platform} has empty hash");
        assert_eq!(hash.len(), 64, "{platform} hash should be 64 hex chars");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{platform} hash is not hex"
        );
    }
    for p in ["mac-x64", "mac-arm64", "linux64", "win64"] {
        assert!(expected_for(p).is_some(), "missing entry for {p}");
    }
}

#[test]
fn verify_accepts_matching_hash() {
    let arm64 = expected_for("mac-arm64").unwrap();
    assert!(verify_checksum("mac-arm64", arm64).is_ok());
}

#[test]
fn verify_rejects_mismatched_hash() {
    // Take a real hash, mutate the last char.
    let real = expected_for("mac-arm64").unwrap();
    let mut bad: String = real.to_string();
    let last = bad.pop().unwrap();
    let replacement = if last == '0' { '1' } else { '0' };
    bad.push(replacement);
    let err = verify_checksum("mac-arm64", &bad).unwrap_err();
    assert!(err.contains("SHA-256 mismatch"), "err should mention mismatch: {err}");
    assert!(err.contains(real), "err should include expected hash");
    assert!(err.contains(&bad), "err should include observed hash");
}

#[test]
fn verify_rejects_unknown_platform() {
    let err = verify_checksum("freebsd-amd64", "deadbeef").unwrap_err();
    assert!(err.contains("unknown"), "err should mention unknown platform: {err}");
}

#[test]
fn verify_handles_case_sensitivity() {
    // SHA-256 hex should be case-insensitive in spirit; our table is lowercase.
    // Decide: require exact match (current impl), or normalize. Pick one and stick.
    // Current impl: exact match. Test it.
    let real = expected_for("linux64").unwrap();
    let upper = real.to_ascii_uppercase();
    let result = verify_checksum("linux64", &upper);
    // Document the choice: lowercase required.
    assert!(
        result.is_err(),
        "uppercase hashes should NOT match (current contract)"
    );
}