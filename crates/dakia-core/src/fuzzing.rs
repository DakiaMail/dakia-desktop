//! Opt-in byte-oriented entrypoints for the standalone `cargo-fuzz` crate.
//!
//! These functions are intentionally absent from normal Dakia builds. They
//! only drive existing production MIME parsing paths and return bounded scalar
//! observations, so a fuzz target never needs storage, a network connection,
//! or a provider account.

#[doc(hidden)]
pub use crate::mail::{
    fuzz_parse_mime_paths, FuzzMimeParsePaths, FuzzMimePathStats, FUZZ_MAX_RETURNED_METADATA_BYTES,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_mime_paths_are_exercised_without_runtime_dependencies() {
        let paths = fuzz_parse_mime_paths(
            b"Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: sender@example.test\r\nSubject: fuzz fixture\r\n\r\nbody",
        );
        assert!(paths.complete.accepted);
        assert!(paths.headers.accepted);
        assert!(paths.catalogue.accepted);
        for path in [paths.complete, paths.headers, paths.catalogue] {
            assert!(path.metadata_bytes <= FUZZ_MAX_RETURNED_METADATA_BYTES);
            assert!(path.attachment_count <= path.part_count.max(1));
        }
    }
}
