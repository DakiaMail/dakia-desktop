//! Resource limits for parsing untrusted RFC 5322 and MIME messages.
//!
//! Preflight checks bound the raw message, outer headers, and obvious MIME
//! boundary storms before the complete MIME tree is allocated. Callers also
//! pass parser-derived metrics to [`validate_structure`] for authoritative
//! aggregate header, part-count, and nesting validation.

use std::{collections::HashSet, fmt};

/// Largest accepted raw RFC 5322 message, before MIME parsing.
pub const MAX_RAW_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
/// Largest accepted aggregate MIME header bytes.
pub const MAX_MIME_HEADER_BYTES: usize = 1024 * 1024;
/// Largest accepted number of MIME parts, after MIME parsing.
pub const MAX_MIME_PARTS: usize = 1_000;
/// Largest accepted multipart nesting depth, after MIME parsing.
pub const MAX_MULTIPART_NESTING: usize = 64;

/// Stable failures emitted when an untrusted message exceeds a parsing budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MimeBudgetError {
    RawMessageTooLarge,
    MimeHeadersTooLarge,
    TooManyMimeParts,
    MultipartNestingTooDeep,
}

impl MimeBudgetError {
    /// A machine-readable error code suitable for callers that need stable
    /// behavior without matching display text.
    pub const fn code(self) -> &'static str {
        match self {
            Self::RawMessageTooLarge => "mime_raw_message_too_large",
            Self::MimeHeadersTooLarge => "mime_headers_too_large",
            Self::TooManyMimeParts => "mime_too_many_parts",
            Self::MultipartNestingTooDeep => "mime_multipart_nesting_too_deep",
        }
    }
}

impl fmt::Display for MimeBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for MimeBudgetError {}

/// Measurements collected without parsing or traversing MIME structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMessagePreflight {
    pub raw_bytes: usize,
    /// Bytes in the outer header section, excluding its terminating blank line.
    pub header_bytes: usize,
}

/// Checks raw-message, outer-header, and obvious MIME part-count budgets before
/// invoking a MIME parser.
///
/// The header scanner accepts both RFC-style CRLF and tolerant LF-only blank
/// lines. If no header/body separator exists, the full raw message is treated
/// as headers. It deliberately does not scan MIME boundaries: body content can
/// resemble headers, so authoritative structural metrics still come from the
/// chosen parser.
pub fn preflight_raw_message(raw: &[u8]) -> Result<RawMessagePreflight, MimeBudgetError> {
    if raw.len() > MAX_RAW_MESSAGE_BYTES {
        return Err(MimeBudgetError::RawMessageTooLarge);
    }

    let header_bytes = outer_header_len(raw);
    validate_header_bytes(header_bytes)?;
    preflight_mime_part_count(raw)?;

    Ok(RawMessagePreflight {
        raw_bytes: raw.len(),
        header_bytes,
    })
}

/// Rejects a boundary storm before the MIME parser allocates every part.
///
/// This is intentionally a conservative framing pass, not a second MIME
/// parser. It discovers declared multipart boundaries on Content-Type lines
/// and counts matching opening delimiters. Post-parse validation remains the
/// source of truth for folded/obscure declarations and exact nesting.
fn preflight_mime_part_count(raw: &[u8]) -> Result<(), MimeBudgetError> {
    let mut boundaries = HashSet::<Vec<u8>>::new();
    let mut too_many_boundaries = false;
    let mut in_headers = true;
    let mut content_type = Vec::new();
    let mut collecting_content_type = false;
    let mut content_type_over_budget = false;
    let mut opening_delimiters = 0;
    let mut over_budget = false;
    for_each_line(raw, |line| {
        if too_many_boundaries || over_budget || content_type_over_budget {
            return;
        }

        if in_headers {
            if line.is_empty() {
                record_mime_boundary(&mut content_type, &mut boundaries, &mut too_many_boundaries);
                collecting_content_type = false;
                in_headers = false;
            } else if line
                .first()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
                && collecting_content_type
            {
                let continuation = trim_ascii_whitespace(line);
                if content_type
                    .len()
                    .checked_add(1 + continuation.len())
                    .is_some_and(|length| length <= MAX_MIME_HEADER_BYTES)
                {
                    content_type.push(b' ');
                    content_type.extend_from_slice(continuation);
                } else {
                    content_type_over_budget = true;
                }
            } else {
                record_mime_boundary(&mut content_type, &mut boundaries, &mut too_many_boundaries);
                collecting_content_type = starts_ascii_case_insensitive(line, b"content-type:");
                if collecting_content_type {
                    content_type.extend_from_slice(line);
                }
            }
            return;
        }

        let delimiter = trim_ascii_end(line);
        let Some(candidate) = delimiter.strip_prefix(b"--") else {
            return;
        };
        if boundaries.contains(candidate) {
            opening_delimiters += 1;
            if opening_delimiters >= MAX_MIME_PARTS {
                over_budget = true;
            } else {
                in_headers = true;
                collecting_content_type = false;
                content_type.clear();
            }
            return;
        }
        if let Some(closing) = candidate.strip_suffix(b"--") {
            boundaries.remove(closing);
        }
    });
    if content_type_over_budget {
        return Err(MimeBudgetError::MimeHeadersTooLarge);
    }
    if too_many_boundaries {
        return Err(MimeBudgetError::TooManyMimeParts);
    }
    if over_budget {
        Err(MimeBudgetError::TooManyMimeParts)
    } else {
        Ok(())
    }
}

fn record_mime_boundary(
    content_type: &mut Vec<u8>,
    boundaries: &mut HashSet<Vec<u8>>,
    too_many_boundaries: &mut bool,
) {
    if let Some(boundary) = mime_boundary_parameter(content_type) {
        if !boundary.is_empty() && boundary.len() <= 200 {
            boundaries.insert(boundary);
            if boundaries.len() >= MAX_MIME_PARTS {
                *too_many_boundaries = true;
            }
        }
    }
    content_type.clear();
}

fn for_each_line(raw: &[u8], mut visit: impl FnMut(&[u8])) {
    let mut start = 0;
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'\n' {
            let end = if index > start && raw[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            visit(&raw[start..end]);
            start = index + 1;
        } else if raw[index] == b'\r' && raw.get(index + 1) != Some(&b'\n') {
            visit(&raw[start..index]);
            start = index + 1;
        }
        index += 1;
    }
    if start < raw.len() {
        visit(&raw[start..]);
    }
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[1..];
    }
    while value.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_ascii_end(mut value: &[u8]) -> &[u8] {
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn starts_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle))
}

fn mime_boundary_parameter(line: &[u8]) -> Option<Vec<u8>> {
    let parameters = multipart_parameter_section(line)?;
    let mut remaining = parameters;

    while let Some((parameter, rest)) = next_parameter(remaining) {
        if let Some(boundary) = boundary_parameter_value(parameter) {
            return Some(boundary);
        }
        remaining = rest;
    }

    None
}

/// Returns the parameter portion of a multipart Content-Type header. This
/// avoids treating boundary-like text in another media type as a delimiter.
fn multipart_parameter_section(line: &[u8]) -> Option<&[u8]> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    let value = trim_ascii_whitespace(&line[colon + 1..]);
    let (media_type, after_media_type) = ascii_token(value);
    if !media_type.eq_ignore_ascii_case(b"multipart") {
        return None;
    }

    let after_slash = trim_ascii_whitespace(after_media_type).strip_prefix(b"/")?;
    let (subtype, parameters) = ascii_token(trim_ascii_whitespace(after_slash));
    (!subtype.is_empty()).then_some(parameters)
}

/// Splits the next parameter at a semicolon that is not inside a quoted value.
fn next_parameter(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let start = value.iter().position(|byte| *byte == b';')? + 1;
    let value = &value[start..];
    let mut quoted = false;
    let mut escaped = false;

    for (index, byte) in value.iter().enumerate() {
        match *byte {
            b'\\' if quoted => escaped = !escaped,
            b'"' if !escaped => quoted = !quoted,
            b';' if !quoted => return Some((&value[..index], &value[index..])),
            _ => escaped = false,
        }
    }

    Some((value, &[]))
}

fn boundary_parameter_value(parameter: &[u8]) -> Option<Vec<u8>> {
    let parameter = trim_ascii_whitespace(parameter);
    let (name, after_name) = ascii_token(parameter);
    if !name.eq_ignore_ascii_case(b"boundary") {
        return None;
    }

    let value = trim_ascii_whitespace(after_name).strip_prefix(b"=")?;
    let value = trim_ascii_whitespace(value);
    if let Some(value) = value.strip_prefix(b"\"") {
        unquote_boundary(value)
    } else {
        let end = value
            .iter()
            .position(|byte| byte.is_ascii_whitespace())
            .unwrap_or(value.len());
        Some(value[..end].to_vec())
    }
}

fn ascii_token(value: &[u8]) -> (&[u8], &[u8]) {
    let end = value
        .iter()
        .position(|byte| matches!(*byte, b' ' | b'\t' | b'/' | b';' | b'='))
        .unwrap_or(value.len());
    (&value[..end], &value[end..])
}

fn unquote_boundary(value: &[u8]) -> Option<Vec<u8>> {
    let mut boundary = Vec::with_capacity(value.len());
    let mut escaped = false;

    for byte in value {
        if escaped {
            boundary.push(*byte);
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(boundary);
        } else {
            boundary.push(*byte);
        }
    }

    None
}

/// Checks a parser-derived aggregate of all MIME header bytes.
///
/// [`preflight_raw_message`] applies the same limit to the outer header block
/// before parsing. Call this after parsing when the MIME tree can provide the
/// aggregate across every part without a second parser.
pub fn validate_header_bytes(header_bytes: usize) -> Result<(), MimeBudgetError> {
    if header_bytes > MAX_MIME_HEADER_BYTES {
        return Err(MimeBudgetError::MimeHeadersTooLarge);
    }
    Ok(())
}

/// Checks parser-derived MIME structure metrics after parsing.
///
/// `part_count` counts the parsed root as one part. `multipart_depth` is zero
/// for a non-multipart root, one for one multipart container, and so on.
pub fn validate_structure(
    part_count: usize,
    multipart_depth: usize,
) -> Result<(), MimeBudgetError> {
    if part_count > MAX_MIME_PARTS {
        return Err(MimeBudgetError::TooManyMimeParts);
    }
    if multipart_depth > MAX_MULTIPART_NESTING {
        return Err(MimeBudgetError::MultipartNestingTooDeep);
    }
    Ok(())
}

fn outer_header_len(raw: &[u8]) -> usize {
    let mut line_start = 0;
    for (index, byte) in raw.iter().enumerate() {
        if *byte == b'\n' {
            let content_end = if index > line_start && raw[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            if content_end == line_start {
                return line_start;
            }
            line_start = index + 1;
        } else if *byte == b'\r' && raw.get(index + 1) != Some(&b'\n') {
            if index == line_start {
                return line_start;
            }
            line_start = index + 1;
        }
    }
    raw.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    #[test]
    fn accepts_messages_at_all_budgets() {
        let raw = b"Subject: budget\r\n\r\nbody";

        assert_eq!(
            preflight_raw_message(raw),
            Ok(RawMessagePreflight {
                raw_bytes: raw.len(),
                header_bytes: b"Subject: budget\r\n".len(),
            })
        );
        assert_eq!(
            validate_structure(MAX_MIME_PARTS, MAX_MULTIPART_NESTING),
            Ok(())
        );
    }

    #[test]
    fn preflight_stops_at_the_outer_header_separator() {
        let raw = b"Subject: hi\n\nbody\n\nnot another header section";

        assert_eq!(
            preflight_raw_message(raw),
            Ok(RawMessagePreflight {
                raw_bytes: raw.len(),
                header_bytes: b"Subject: hi\n".len(),
            })
        );
    }

    #[test]
    fn preflight_stops_at_a_cr_only_outer_header_separator() {
        let mut raw = b"Subject: hi\r\r".to_vec();
        raw.resize(MAX_MIME_HEADER_BYTES + 128, b'x');

        assert_eq!(
            preflight_raw_message(&raw),
            Ok(RawMessagePreflight {
                raw_bytes: raw.len(),
                header_bytes: b"Subject: hi\r".len(),
            })
        );
    }

    #[test]
    fn preflight_accepts_an_empty_outer_header_section() {
        assert_eq!(
            preflight_raw_message(b"\r\nbody"),
            Ok(RawMessagePreflight {
                raw_bytes: b"\r\nbody".len(),
                header_bytes: 0,
            })
        );
    }

    #[test]
    fn preflight_accepts_a_header_at_the_exact_limit() {
        let raw = vec![b'a'; MAX_MIME_HEADER_BYTES];

        assert_eq!(
            preflight_raw_message(&raw),
            Ok(RawMessagePreflight {
                raw_bytes: MAX_MIME_HEADER_BYTES,
                header_bytes: MAX_MIME_HEADER_BYTES,
            })
        );
    }

    #[test]
    fn preflight_rejects_oversized_outer_headers_with_a_stable_error() {
        let raw = vec![b'a'; MAX_MIME_HEADER_BYTES + 1];

        assert_eq!(
            preflight_raw_message(&raw),
            Err(MimeBudgetError::MimeHeadersTooLarge)
        );
        assert_eq!(
            MimeBudgetError::MimeHeadersTooLarge.code(),
            "mime_headers_too_large"
        );
    }

    #[test]
    fn aggregate_headers_share_the_preflight_limit() {
        assert_eq!(validate_header_bytes(MAX_MIME_HEADER_BYTES), Ok(()));
        assert_eq!(
            validate_header_bytes(MAX_MIME_HEADER_BYTES + 1),
            Err(MimeBudgetError::MimeHeadersTooLarge)
        );
    }

    #[test]
    fn preflight_rejects_oversized_raw_messages_before_header_scanning() {
        let raw = vec![b'a'; MAX_RAW_MESSAGE_BYTES + 1];

        assert_eq!(
            preflight_raw_message(&raw),
            Err(MimeBudgetError::RawMessageTooLarge)
        );
        assert_eq!(
            MimeBudgetError::RawMessageTooLarge.to_string(),
            "mime_raw_message_too_large"
        );
    }

    #[test]
    fn preflight_rejects_a_boundary_storm_before_complete_parsing() {
        fn multipart(leaves: usize) -> Vec<u8> {
            let mut raw = b"Content-Type: multipart/mixed; boundary=\"storm--\"\r\n\r\n".to_vec();
            for _ in 0..leaves {
                raw.extend_from_slice(b"--storm--\r\n\r\nx\r\n");
            }
            raw.extend_from_slice(b"--storm----\r\n");
            raw
        }

        assert!(preflight_raw_message(&multipart(MAX_MIME_PARTS - 1)).is_ok());
        assert_eq!(
            preflight_raw_message(&multipart(MAX_MIME_PARTS)),
            Err(MimeBudgetError::TooManyMimeParts)
        );
    }

    #[test]
    fn mail_parser_accepts_boundary_with_whitespace_around_equals() {
        let raw = b"Content-Type: multipart/mixed; boundary = storm\r\n\r\n\
--storm\r\n\r\nbody\r\n\
--storm--\r\n";

        assert!(MessageParser::default().parse(raw).unwrap().parts.len() > 1);
    }

    #[test]
    fn preflight_rejects_boundary_storms_with_tolerant_parameter_spacing() {
        fn multipart(declaration: &[u8], boundary: &[u8], leaves: usize) -> Vec<u8> {
            let mut raw = b"Content-Type: multipart/mixed; ".to_vec();
            raw.extend_from_slice(declaration);
            raw.extend_from_slice(b"\r\n\r\n");
            for _ in 0..leaves {
                raw.extend_from_slice(b"--");
                raw.extend_from_slice(boundary);
                raw.extend_from_slice(b"\r\n\r\nx\r\n");
            }
            raw.extend_from_slice(b"--");
            raw.extend_from_slice(boundary);
            raw.extend_from_slice(b"--\r\n");
            raw
        }

        for (declaration, boundary) in [
            (b"boundary = storm".as_slice(), b"storm".as_slice()),
            (
                b"BOUNDARY\t=\t\"quoted-token\"".as_slice(),
                b"quoted-token".as_slice(),
            ),
            (b"boundary = ending--".as_slice(), b"ending--".as_slice()),
        ] {
            assert_eq!(
                preflight_raw_message(&multipart(declaration, boundary, MAX_MIME_PARTS)),
                Err(MimeBudgetError::TooManyMimeParts),
                "failed to preflight {declaration:?}"
            );
        }
    }

    #[test]
    fn preflight_finds_a_boundary_after_a_long_folded_content_type_parameter() {
        let mut raw = b"Content-Type: multipart/mixed;\r\n\tnote=\"".to_vec();
        raw.extend(std::iter::repeat_n(b'x', 8_192));
        raw.extend_from_slice(b"\";\r\n\tboundary = storm\r\n\r\n");
        for _ in 0..MAX_MIME_PARTS {
            raw.extend_from_slice(b"--storm\r\n\r\nx\r\n");
        }
        raw.extend_from_slice(b"--storm--\r\n");

        assert_eq!(
            preflight_raw_message(&raw),
            Err(MimeBudgetError::TooManyMimeParts)
        );
    }

    #[test]
    fn preflight_rejects_an_oversized_inner_content_type_before_parsing() {
        let mut raw =
            b"Content-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\nContent-Type: multipart/mixed;\r\n\tpadding=\""
                .to_vec();
        raw.extend(std::iter::repeat_n(b'x', MAX_MIME_HEADER_BYTES));
        raw.extend_from_slice(b"\"; boundary=inner\r\n\r\n--inner--\r\n--outer--\r\n");

        assert_eq!(
            preflight_raw_message(&raw),
            Err(MimeBudgetError::MimeHeadersTooLarge)
        );
    }

    #[test]
    fn preflight_does_not_parse_quoted_mime_inside_plain_text() {
        let mut raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\n".to_vec();
        raw.extend_from_slice(b"Content-Type: multipart/mixed; boundary = storm\r\n");
        for _ in 0..MAX_MIME_PARTS {
            raw.extend_from_slice(b"--storm\r\n");
        }

        assert!(preflight_raw_message(&raw).is_ok());
    }

    #[test]
    fn preflight_does_not_treat_quoted_parameter_text_as_a_boundary() {
        let mut raw =
            b"Content-Type: multipart/mixed; note=\"boundary = storm\"; boundary=real\r\n\r\n"
                .to_vec();
        for _ in 0..MAX_MIME_PARTS {
            raw.extend_from_slice(b"--storm\r\n");
        }

        assert!(preflight_raw_message(&raw).is_ok());
    }

    #[test]
    fn structure_validator_has_stable_part_and_depth_failures() {
        assert_eq!(
            validate_structure(MAX_MIME_PARTS + 1, 0),
            Err(MimeBudgetError::TooManyMimeParts)
        );
        assert_eq!(
            validate_structure(1, MAX_MULTIPART_NESTING + 1),
            Err(MimeBudgetError::MultipartNestingTooDeep)
        );
        assert_eq!(
            MimeBudgetError::TooManyMimeParts.code(),
            "mime_too_many_parts"
        );
        assert_eq!(
            MimeBudgetError::MultipartNestingTooDeep.code(),
            "mime_multipart_nesting_too_deep"
        );
    }
}
