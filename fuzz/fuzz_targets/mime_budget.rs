#![no_main]

use dakia_core::fuzzing::{fuzz_parse_mime_paths, FUZZ_MAX_RETURNED_METADATA_BYTES};
use libfuzzer_sys::fuzz_target;

// Drive the actual complete, headers-only, and catalogue MIME paths. Parse
// errors are valid for arbitrary bytes; accepted output must remain bounded.
fuzz_target!(|input: &[u8]| {
    let paths = fuzz_parse_mime_paths(input);
    for path in [paths.complete, paths.headers, paths.catalogue] {
        assert!(path.metadata_bytes <= FUZZ_MAX_RETURNED_METADATA_BYTES);
        assert!(path.attachment_count <= path.part_count.max(1));
    }
});
