#![no_main]

use dakia_core::mail::{is_potentially_unsafe, safe_attachment_filename, safe_mime_type};
use libfuzzer_sys::fuzz_target;

// Exercise the public attachment-metadata boundary with arbitrary bytes. A
// string is formed lossily so invalid UTF-8 is part of the input space too.
fuzz_target!(|input: &[u8]| {
    let value = String::from_utf8_lossy(input);
    let filename = safe_attachment_filename(&value, input.len());
    let mime_type = safe_mime_type(&value);
    let _ = is_potentially_unsafe(&filename, &mime_type);
});
