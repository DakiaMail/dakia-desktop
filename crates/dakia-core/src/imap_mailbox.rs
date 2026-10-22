//! Strict IMAP4rev1 modified UTF-7 mailbox-name codec.
//!
//! The codec deliberately accepts only the canonical form described by RFC
//! 3501 section 5.1.3.  A mailbox name is protocol data, so accepting
//! equivalent alternate spellings would make remote-name identity ambiguous.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum ModifiedUtf7Error {
    #[error("IMAP mailbox name contains a control or protocol-framing character")]
    ControlCharacter,
    #[error("IMAP mailbox name contains a non-ASCII wire character")]
    NonAsciiWireCharacter,
    #[error("IMAP mailbox name has an unterminated modified UTF-7 shift")]
    UnterminatedShift,
    #[error("IMAP mailbox name has an empty or superfluous modified UTF-7 shift")]
    SuperfluousShift,
    #[error("IMAP mailbox name contains invalid modified UTF-7 base64")]
    InvalidBase64,
    #[error("IMAP mailbox name contains an invalid UTF-16 sequence")]
    InvalidUtf16,
    #[error("IMAP mailbox name uses a non-canonical modified UTF-7 encoding")]
    NonCanonical,
}

/// Encodes a logical mailbox name as canonical IMAP4rev1 modified UTF-7.
///
/// Printable US-ASCII remains literal, except `&`, which is represented as
/// `&-`.  Non-ASCII runs are UTF-16BE, standard base64 without padding, with
/// `/` replaced by `,`.
pub(crate) fn encode_modified_utf7(name: &str) -> Result<String, ModifiedUtf7Error> {
    let mut encoded = String::with_capacity(name.len());
    let mut shifted = Vec::<u16>::new();

    for character in name.chars() {
        if is_disallowed_logical_character(character) {
            return Err(ModifiedUtf7Error::ControlCharacter);
        }

        if is_printable_ascii(character) {
            flush_shift(&mut encoded, &mut shifted);
            if character == '&' {
                encoded.push_str("&-");
            } else {
                encoded.push(character);
            }
        } else {
            let mut units = [0_u16; 2];
            shifted.extend_from_slice(character.encode_utf16(&mut units));
        }
    }

    flush_shift(&mut encoded, &mut shifted);
    Ok(encoded)
}

/// Decodes a canonical IMAP4rev1 modified UTF-7 wire mailbox name.
///
/// This rejects malformed shifts, alternate base64 spellings, shifted ASCII,
/// controls, and raw non-ASCII bytes.  The exact remote wire representation
/// should still be persisted separately for identity and protocol commands.
pub(crate) fn decode_modified_utf7(wire_name: &str) -> Result<String, ModifiedUtf7Error> {
    let bytes = wire_name.as_bytes();
    for byte in bytes {
        if !byte.is_ascii() {
            return Err(ModifiedUtf7Error::NonAsciiWireCharacter);
        }
        if !(0x20..=0x7e).contains(byte) {
            return Err(ModifiedUtf7Error::ControlCharacter);
        }
    }

    let mut decoded = String::with_capacity(wire_name.len());
    let mut index = 0;
    let mut previous_was_non_ascii_shift = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii() {
            return Err(ModifiedUtf7Error::NonAsciiWireCharacter);
        }
        if !(0x20..=0x7e).contains(&byte) {
            return Err(ModifiedUtf7Error::ControlCharacter);
        }

        if byte != b'&' {
            decoded.push(char::from(byte));
            previous_was_non_ascii_shift = false;
            index += 1;
            continue;
        }

        let shift_start = index;
        index += 1;
        let shift_end = bytes[index..]
            .iter()
            .position(|candidate| *candidate == b'-')
            .map(|relative| index + relative)
            .ok_or(ModifiedUtf7Error::UnterminatedShift)?;

        if shift_end == index {
            decoded.push('&');
            previous_was_non_ascii_shift = false;
            index = shift_end + 1;
            continue;
        }

        if previous_was_non_ascii_shift {
            return Err(ModifiedUtf7Error::SuperfluousShift);
        }

        let shifted = &wire_name[index..shift_end];
        let mut standard_base64 = shifted.replace(',', "/");
        if standard_base64.contains('=') {
            return Err(ModifiedUtf7Error::NonCanonical);
        }

        let raw = STANDARD_NO_PAD
            .decode(&standard_base64)
            .map_err(|_| ModifiedUtf7Error::InvalidBase64)?;
        if raw.is_empty() || raw.len() % 2 != 0 {
            return Err(ModifiedUtf7Error::InvalidUtf16);
        }

        let units = raw
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let text = String::from_utf16(&units).map_err(|_| ModifiedUtf7Error::InvalidUtf16)?;

        if text.chars().any(is_disallowed_logical_character) {
            return Err(ModifiedUtf7Error::ControlCharacter);
        }
        if text.chars().any(is_printable_ascii) {
            return Err(ModifiedUtf7Error::NonCanonical);
        }

        standard_base64 = STANDARD_NO_PAD.encode(raw);
        if standard_base64.replace('/', ",") != shifted {
            return Err(ModifiedUtf7Error::NonCanonical);
        }

        decoded.push_str(&text);
        previous_was_non_ascii_shift = true;
        index = shift_end + 1;

        debug_assert_eq!(bytes[shift_start], b'&');
    }

    Ok(decoded)
}

fn flush_shift(output: &mut String, utf16_units: &mut Vec<u16>) {
    if utf16_units.is_empty() {
        return;
    }

    let mut bytes = Vec::with_capacity(utf16_units.len() * 2);
    for unit in utf16_units.drain(..) {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    output.push('&');
    output.push_str(&STANDARD_NO_PAD.encode(bytes).replace('/', ","));
    output.push('-');
}

fn is_printable_ascii(character: char) -> bool {
    matches!(character, '\u{20}'..='\u{7e}')
}

fn is_disallowed_logical_character(character: char) -> bool {
    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::{decode_modified_utf7, encode_modified_utf7, ModifiedUtf7Error};

    #[test]
    fn encodes_rfc_3501_vectors_and_protocol_safe_ascii() {
        assert_eq!(encode_modified_utf7("日本語").unwrap(), "&ZeVnLIqe-");
        assert_eq!(
            encode_modified_utf7("~peter/mail/台北/日本語").unwrap(),
            "~peter/mail/&U,BTFw-/&ZeVnLIqe-"
        );
        assert_eq!(encode_modified_utf7("a&b").unwrap(), "a&-b");
        assert_eq!(
            encode_modified_utf7("quoted \\\"name\\\"").unwrap(),
            "quoted \\\"name\\\""
        );
        assert_eq!(
            encode_modified_utf7("台北日本語").unwrap(),
            "&U,BTF2XlZyyKng-"
        );
        assert_eq!(encode_modified_utf7("😀").unwrap(), "&2D3eAA-");
    }

    #[test]
    fn strict_decode_round_trips_canonical_vectors() {
        assert_eq!(decode_modified_utf7("").unwrap(), "");
        assert_eq!(encode_modified_utf7("").unwrap(), "");
        for (wire, logical) in [
            ("&ZeVnLIqe-", "日本語"),
            ("~peter/mail/&U,BTFw-/&ZeVnLIqe-", "~peter/mail/台北/日本語"),
            ("a&-b", "a&b"),
            ("&U,BTF2XlZyyKng-", "台北日本語"),
            ("&2D3eAA-", "😀"),
        ] {
            assert_eq!(decode_modified_utf7(wire).unwrap(), logical);
            assert_eq!(encode_modified_utf7(logical).unwrap(), wire);
        }
    }

    #[test]
    fn rejects_controls_and_protocol_framing_on_both_boundaries() {
        for value in [
            "Inbox\r\nA001 DELETE",
            "Inbox\0",
            "Inbox\u{7f}",
            "Inbox\u{2028}",
        ] {
            assert_eq!(
                encode_modified_utf7(value).unwrap_err(),
                ModifiedUtf7Error::ControlCharacter
            );
        }
        for value in [
            "Inbox\r\nA001 DELETE",
            "Inbox\0",
            "Inbox\u{7f}",
            "Inbox&ICg-",
        ] {
            assert_eq!(
                decode_modified_utf7(value).unwrap_err(),
                ModifiedUtf7Error::ControlCharacter
            );
        }
        assert_eq!(
            decode_modified_utf7("Café").unwrap_err(),
            ModifiedUtf7Error::NonAsciiWireCharacter
        );
    }

    #[test]
    fn rejects_malformed_noncanonical_and_invalid_utf16_shifts() {
        for value in [
            "&",                  // unterminated
            "&Jjo!",              // missing shift terminator
            "&-",                 // literal ampersand only, checked separately
            "&U,BTFw-&ZeVnLIqe-", // adjacent shifts
            "&AGE-",              // shifted printable ASCII 'a'
            "&AAA-",              // shifted NUL
            "&2AA-",              // lone high surrogate
            "&3AA-",              // lone low surrogate
            "&ZeVnLIqe",          // missing terminator
            "&ZeVnLIqe=-",        // padded alternate form
            "&ZeVnLIqe--",        // trailing literal hyphen is valid, checked separately
        ] {
            if matches!(value, "&-" | "&ZeVnLIqe--") {
                continue;
            }
            assert!(decode_modified_utf7(value).is_err(), "{value}");
        }
        assert_eq!(decode_modified_utf7("&-").unwrap(), "&");
        assert_eq!(decode_modified_utf7("&-&ZeVnLIqe-").unwrap(), "&日本語");
        assert_eq!(decode_modified_utf7("&-&-").unwrap(), "&&");
        assert_eq!(decode_modified_utf7("&ZeVnLIqe--").unwrap(), "日本語-");
    }

    #[test]
    fn rejects_shifted_ascii_even_when_the_base64_is_valid() {
        assert_eq!(
            decode_modified_utf7("&ACY-").unwrap_err(),
            ModifiedUtf7Error::NonCanonical
        );
        assert_eq!(
            decode_modified_utf7("&AEE-").unwrap_err(),
            ModifiedUtf7Error::NonCanonical
        );
    }

    #[test]
    fn generated_representative_names_round_trip_as_ascii_unpadded_wire() {
        let scalars = ["A", "&", "é", "台", "日", "😀", "👩", "💻", "~", "\\", "\""];
        for left in scalars {
            for right in scalars {
                let logical = format!("prefix{left}{right}suffix");
                let wire = encode_modified_utf7(&logical).unwrap();
                assert!(wire.bytes().all(|byte| (0x20..=0x7e).contains(&byte)));
                assert!(!wire.contains(['=', '/']));
                assert_eq!(decode_modified_utf7(&wire).unwrap(), logical);
            }
        }
        assert_eq!(decode_modified_utf7("+plain-").unwrap(), "+plain-");
    }
}
