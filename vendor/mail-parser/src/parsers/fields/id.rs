/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::{parsers::MessageStream, HeaderValue};
use std::borrow::Cow;

impl<'x> MessageStream<'x> {
    pub fn parse_id(&mut self) -> HeaderValue<'x> {
        let mut token_start: usize = 0;
        let mut token_end: usize = 0;
        let mut token_invalid_start: usize = 0; // Handle broken clients
        let mut token_invalid_end: usize = 0; // Handle broken clients
        let mut is_id_part = false;
        let mut ids = Vec::new();

        while let Some(&ch) = self.next() {
            match ch {
                b'\n' => {
                    if !self.try_next_is_space() {
                        return match ids.len() {
                            1 => HeaderValue::Text(ids.pop().unwrap()),
                            0 => {
                                if token_invalid_start > 0 {
                                    HeaderValue::Text(String::from_utf8_lossy(
                                        self.bytes(token_invalid_start - 1..token_invalid_end),
                                    ))
                                } else {
                                    HeaderValue::Empty
                                }
                            }
                            _ => HeaderValue::TextList(ids),
                        };
                    } else {
                        continue;
                    }
                }
                b'<' => {
                    is_id_part = true;
                    continue;
                }
                b'>' => {
                    is_id_part = false;
                    if token_start > 0 {
                        ids.push(normalize_obs_message_id(
                            self.bytes(token_start - 1..token_end),
                        ));
                        token_start = 0;
                    } else {
                        continue;
                    }
                }
                b' ' | b'\t' | b'\r' => continue,
                _ => {}
            }
            if is_id_part {
                if token_start == 0 {
                    token_start = self.offset();
                }
                token_end = self.offset();
            } else {
                if token_invalid_start == 0 {
                    token_invalid_start = self.offset();
                }
                token_invalid_end = self.offset();
            }
        }

        HeaderValue::Empty
    }
}

/// RFC 5322 section 4.5.4 permits CFWS between the atoms of an obsolete
/// message identifier. CFWS is not part of the semantic identifier, while
/// quoted-string contents and escaped bytes remain untouched.
fn normalize_obs_message_id<'x>(bytes: &'x [u8]) -> Cow<'x, str> {
    if !bytes
        .iter()
        .any(|byte| matches!(*byte, b' ' | b'\t' | b'\r' | b'\n' | b'('))
    {
        return String::from_utf8_lossy(bytes);
    }

    let Some(normalized) = canonical_obs_message_id(bytes) else {
        return String::from_utf8_lossy(bytes);
    };
    String::from_utf8_lossy(&normalized).into_owned().into()
}

fn canonical_obs_message_id(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut comment_depth = 0usize;
    let mut quoted = false;
    let mut literal = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            normalized.push(byte);
            match byte {
                b'\\' if !escaped => escaped = true,
                b'"' if !escaped => quoted = false,
                _ => escaped = false,
            }
            index += 1;
            continue;
        }
        if literal {
            normalized.push(byte);
            match byte {
                b'\\' if !escaped => escaped = true,
                b']' if !escaped => literal = false,
                _ => escaped = false,
            }
            index += 1;
            continue;
        }
        match byte {
            b'(' | b' ' | b'\t' | b'\r' | b'\n' => {
                let previous = *normalized.last()?;
                let next = consume_cfws(bytes, &mut index, &mut comment_depth, &mut escaped)?;
                if !valid_cfws_position(previous, next) {
                    return None;
                }
                continue;
            }
            b'"' => {
                quoted = true;
                normalized.push(byte);
            }
            b'[' => {
                literal = true;
                normalized.push(byte);
            }
            b')' | b'\\' => return None,
            _ => normalized.push(byte),
        }
        index += 1;
    }
    (!quoted && !literal && comment_depth == 0 && !escaped && is_obs_message_id(&normalized))
        .then_some(normalized)
}

fn consume_cfws(
    bytes: &[u8],
    index: &mut usize,
    comment_depth: &mut usize,
    escaped: &mut bool,
) -> Option<u8> {
    loop {
        while matches!(bytes.get(*index), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            *index += 1;
        }
        if bytes.get(*index) != Some(&b'(') {
            return bytes.get(*index).copied();
        }
        *comment_depth = 1;
        *index += 1;
        while *comment_depth > 0 {
            let byte = *bytes.get(*index)?;
            *index += 1;
            if *escaped {
                *escaped = false;
            } else {
                match byte {
                    b'\\' => *escaped = true,
                    b'(' => *comment_depth += 1,
                    b')' => *comment_depth -= 1,
                    _ => {}
                }
            }
        }
        if *escaped {
            return None;
        }
    }
}

fn valid_cfws_position(previous: u8, next: u8) -> bool {
    match previous {
        byte if is_id_atext(byte) => matches!(next, b'.' | b'@'),
        b'.' => is_id_atext(next),
        b'@' => is_id_atext(next) || next == b'[',
        b'"' => next == b'@',
        b']' => next == b'.',
        _ => false,
    }
}

fn is_obs_message_id(value: &[u8]) -> bool {
    let mut quoted = false;
    let mut literal = false;
    let mut escaped = false;
    let mut at = None;
    for (index, byte) in value.iter().copied().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted {
            match byte {
                b'\\' => escaped = true,
                b'"' => quoted = false,
                _ => {}
            }
        } else if literal {
            match byte {
                b'\\' => escaped = true,
                b']' => literal = false,
                _ => {}
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' => literal = true,
                b'@' if at.replace(index).is_some() => return false,
                b'@' => {}
                _ => {}
            }
        }
    }
    let Some(at) = at else { return false };
    let (left, right) = (&value[..at], &value[at + 1..]);
    (is_dot_atom(left) || is_quoted_string(left))
        && (is_dot_atom(right) || is_domain_literal(right))
}

fn is_dot_atom(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .split(|byte| *byte == b'.')
            .all(|atom| !atom.is_empty() && atom.iter().copied().all(is_id_atext))
}

fn is_quoted_string(value: &[u8]) -> bool {
    value.len() >= 2 && value[0] == b'"' && value[value.len() - 1] == b'"'
}

fn is_domain_literal(value: &[u8]) -> bool {
    value.len() >= 2
        && value[0] == b'['
        && value[value.len() - 1] == b']'
        && value[1..value.len() - 1]
            .iter()
            .all(|byte| *byte != b'[' && *byte != b']')
}

fn is_id_atext(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use crate::parsers::{fields::load_tests, MessageStream};

    #[test]
    fn rfc5322_obs_id_cfws_is_not_part_of_the_identifier() {
        let id = MessageStream::new(b"<left (note) . part @ example . test>\r\n")
            .parse_id()
            .into_text_list()
            .expect("one message identifier");
        assert_eq!(id.as_slice(), [Cow::Borrowed("left.part@example.test")]);
    }

    #[test]
    fn encoded_word_looking_message_id_stays_literal() {
        let id = MessageStream::new(b"<=?UTF-8?Q?message?=@example.test>\r\n")
            .parse_id()
            .into_text_list()
            .expect("one message identifier");
        assert_eq!(
            id.as_slice(),
            [Cow::Borrowed("=?UTF-8?Q?message?=@example.test")]
        );
    }

    #[test]
    fn obs_id_cfws_normalization_never_changes_literal_or_invalid_identity() {
        for (input, expected) in [
            ("<left@[foo(bar)]>\r\n", "left@[foo(bar)]"),
            (
                "<\"left (literal)\"@example.test>\r\n",
                "\"left (literal)\"@example.test",
            ),
            (
                "<left (outer (nested\\) note)) . part@example.test>\r\n",
                "left.part@example.test",
            ),
            ("<left right@example.test>\r\n", "left right@example.test"),
            (
                "<left (unclosed@example.test>\r\n",
                "left (unclosed@example.test",
            ),
        ] {
            let id = MessageStream::new(input.as_bytes())
                .parse_id()
                .into_text_list()
                .expect("one message identifier");
            assert_eq!(id.as_slice(), [Cow::Borrowed(expected)], "{input}");
        }
    }

    #[test]
    fn parse_message_ids() {
        for test in load_tests::<Option<Vec<Cow<'static, str>>>>("id.json") {
            assert_eq!(
                MessageStream::new(test.header.as_bytes())
                    .parse_id()
                    .into_text_list(),
                test.expected,
                "failed for {:?}",
                test.header
            );
        }
    }
}
