//! SHA-256 verification for the pinned chrome-headless-shell binary.
//!
//! Called at runtime from `chromium_ensure::download_chromium` after the
//! zip is extracted; the CHECKSUMS table is the source of truth.

use std::env;

/// Hardcoded expected hashes. Empty string = "no pin yet" (escape hatch).
pub const CHECKSUMS: &[(&str, &str)] = &[
    ("mac-x64", "b01ce7b6b2d0a1e343ac2f9f71c948c6735462420478ff8c87f479b23b4d980d"),
    ("mac-arm64", "1671bf74f9d78a4b3a4f1c1d3f33fb5ed30220535656a7db07a845389322f8dd"),
    ("linux64", "64887dbeca3bc2230fa0ec57d20b0a4f2eb9ce039a432a3814270588a3ebc562"),
    ("win64", "639c00793220b678cd010a35276627cd00c998f849c740452451ef419bb32a1a"),
];

/// Look up the expected hash for a platform.
pub fn expected_for(platform: &str) -> Option<&'static str> {
    CHECKSUMS
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, h)| *h)
}

/// Verify the binary's actual SHA-256 against the expected hash.
///
/// - `actual`: lowercase hex SHA-256 of the extracted binary, computed by caller.
/// - `platform`: one of `mac-x64 | mac-arm64 | linux64 | win64`.
/// - On success: returns Ok(()), caller proceeds.
/// - On mismatch: returns Err with an actionable message including both hashes.
/// - On missing-pin: returns Err unless `DRAWIO_AGENT_ACCEPT_NEW_CHECKSUM=1`
///   is set, in which case returns Ok(()) and emits a loud warning.
///
/// Errors are designed to be developer-actionable, not panic messages.
pub fn verify_checksum(platform: &str, actual: &str) -> Result<(), String> {
    let expected = expected_for(platform);
    match expected {
        None => Err(format!(
            "chrome-headless-shell platform {platform:?} is unknown. \
             Add it to CHECKSUMS in crates/renderer/src/checksum.rs."
        )),
        Some("") => {
            // Empty hash = "no pin yet" placeholder (shouldn't happen with the
            // current table; kept as a forward-compat hook for adding new
            // platforms).
            if env::var("DRAWIO_AGENT_ACCEPT_NEW_CHECKSUM").is_ok() {
                eprintln!(
                    "ACCEPTING UNPINNED chrome-headless-shell {platform}: {actual}. \
                     Bake this hash into crates/renderer/src/checksum.rs::CHECKSUMS."
                );
                Ok(())
            } else {
                Err(format!(
                    "no pinned SHA-256 for chrome-headless-shell {platform}.\n\
                     observed: {actual}\n\
                     Set DRAWIO_AGENT_ACCEPT_NEW_CHECKSUM=1 to proceed once, \
                     or paste this hash into CHECKSUMS in crates/renderer/src/checksum.rs."
                ))
            }
        }
        Some(exp) if exp == actual => {
            eprintln!("chrome-headless-shell {platform} SHA-256 verified ✓");
            Ok(())
        }
        Some(exp) => Err(format!(
            "SHA-256 mismatch for chrome-headless-shell {platform}:\n  \
             expected: {exp}\n  observed: {actual}\n\
             The downloaded binary may have been replaced or tampered with. \
             This usually means PINNED_VERSION and CHECKSUMS got out of sync. \
             Bump them together, or set DRAWIO_AGENT_ACCEPT_NEW_CHECKSUM=1 to \
             override (and re-check whether the binary is the one you intended)."
        )),
    }
}