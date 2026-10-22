/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::{parsers::MessageStream, Addr, Address, Group, HeaderValue};
use std::borrow::Cow;

#[derive(PartialEq, Clone, Copy, Debug)]
enum AddressState {
    Address,
    Name,
    Quote,
    Comment,
}

/// A raw bare-address item can either need no obsolete handling, be safely
/// normalized, or be an invalid obsolete candidate. The final state must not
/// fall through to token recovery because that can manufacture a different
/// actionable recipient from a malformed local part.
#[derive(Debug, PartialEq, Eq)]
enum ObsBareAddrSpec {
    NotApplicable,
    Normalized(String),
    InvalidCandidate,
}

pub struct AddressParser<'x> {
    token_start: usize,
    token_end: usize,

    is_token_email: bool,
    is_token_start: bool,
    is_escaped: bool,

    name_tokens: Vec<Cow<'x, str>>,
    mail_tokens: Vec<Cow<'x, str>>,
    comment_tokens: Vec<Cow<'x, str>>,

    state: AddressState,
    state_stack: Vec<AddressState>,

    addresses: Vec<Addr<'x>>,
    group_name: Option<Cow<'x, str>>,
    group_comment: Option<Cow<'x, str>>,
    raw_angle_address: Option<Cow<'x, str>>,
    angle_address_start: Option<usize>,
    item_start: usize,
    result: Vec<Group<'x>>,
}

impl<'x> AddressParser<'x> {
    pub fn add_token(&mut self, stream: &MessageStream<'x>, add_trail_space: bool) {
        if self.token_start > 0 {
            let token = String::from_utf8_lossy(&stream.data[self.token_start - 1..self.token_end]);
            let mut add_space = false;
            let list = match self.state {
                AddressState::Address => &mut self.mail_tokens,
                AddressState::Name => {
                    if self.is_token_email {
                        &mut self.mail_tokens
                    } else {
                        add_space = true;
                        &mut self.name_tokens
                    }
                }
                AddressState::Quote => &mut self.name_tokens,
                AddressState::Comment => {
                    add_space = true;
                    &mut self.comment_tokens
                }
            };

            if add_space && !list.is_empty() {
                list.push(" ".into());
            }

            list.push(token);

            if add_trail_space {
                list.push(" ".into());
            }

            self.token_start = 0;
            self.is_token_email = false;
            self.is_token_start = true;
            self.is_escaped = false;
        }
    }

    pub fn add_address(&mut self, raw_item: Option<&[u8]>) {
        let display_comment = raw_item.and_then(outer_display_comment);
        match raw_item.map(normalize_obs_bare_addr_spec) {
            Some(ObsBareAddrSpec::Normalized(address)) => {
                let name = display_comment.map(Into::into);
                self.name_tokens.clear();
                self.mail_tokens.clear();
                self.comment_tokens.clear();
                self.raw_angle_address = None;
                self.addresses.push(Addr {
                    name,
                    address: Some(address.into()),
                });
                return;
            }
            Some(ObsBareAddrSpec::InvalidCandidate) => {
                let raw = raw_item
                    .and_then(|raw| std::str::from_utf8(raw).ok())
                    .map(|raw| raw.trim().to_owned().into());
                self.name_tokens.clear();
                self.mail_tokens.clear();
                self.comment_tokens.clear();
                self.raw_angle_address = None;
                self.addresses.push(Addr {
                    name: raw,
                    address: None,
                });
                return;
            }
            _ => {}
        }

        let has_mail = !self.mail_tokens.is_empty();
        let has_name = !self.name_tokens.is_empty();
        let has_comment = !self.comment_tokens.is_empty();

        let address = has_mail.then(|| {
            let parsed = concat_tokens(&mut self.mail_tokens);
            match self.raw_angle_address.take() {
                Some(raw) if raw.contains(':') => {
                    let terminal = strip_obs_route(&raw).into_owned();
                    if terminal != raw {
                        terminal.into()
                    } else {
                        raw
                    }
                }
                _ => parsed,
            }
        });
        self.addresses.push(if has_mail && has_name && has_comment {
            Addr {
                name: Some(
                    format!(
                        "{} ({})",
                        concat_tokens(&mut self.name_tokens),
                        concat_tokens(&mut self.comment_tokens)
                    )
                    .into(),
                ),
                address: address.expect("mail tokens checked above").into(),
            }
        } else if has_name && has_mail {
            Addr {
                name: concat_tokens(&mut self.name_tokens).into(),
                address: address.expect("mail tokens checked above").into(),
            }
        } else if has_mail && has_comment {
            Addr {
                name: concat_tokens(&mut self.comment_tokens).into(),
                address: address.expect("mail tokens checked above").into(),
            }
        } else if has_mail {
            Addr {
                name: None,
                address: address.expect("mail tokens checked above").into(),
            }
        } else if has_name && has_comment {
            Addr {
                name: concat_tokens(&mut self.comment_tokens).into(),
                address: concat_tokens(&mut self.name_tokens).into(),
            }
        } else if has_name {
            Addr {
                name: concat_tokens(&mut self.name_tokens).into(),
                address: None,
            }
        } else if has_comment {
            Addr {
                name: concat_tokens(&mut self.comment_tokens).into(),
                address: None,
            }
        } else {
            return;
        });
    }

    pub fn add_group_details(&mut self) {
        if !self.name_tokens.is_empty() {
            self.group_name = concat_tokens(&mut self.name_tokens).into();
        }

        if !self.comment_tokens.is_empty() {
            self.group_comment = concat_tokens(&mut self.comment_tokens).into();
        }

        if !self.mail_tokens.is_empty() {
            if let Some(group_name) = &self.group_name {
                self.group_name =
                    Some(format!("{} {}", group_name, concat_tokens(&mut self.mail_tokens)).into());
            } else {
                self.group_name = concat_tokens(&mut self.mail_tokens).into();
            }
        }
    }

    pub fn add_group(&mut self) {
        let has_name = self.group_name.is_some();
        let has_comment = self.group_comment.is_some();
        let has_addresses = !self.addresses.is_empty();

        self.result
            .push(if has_name && has_addresses && has_comment {
                Group {
                    name: Some(
                        format!(
                            "{} ({})",
                            self.group_name.take().unwrap(),
                            self.group_comment.take().unwrap()
                        )
                        .into(),
                    ),
                    addresses: std::mem::take(&mut self.addresses),
                }
            } else if has_addresses && has_name {
                Group {
                    name: self.group_name.take(),
                    addresses: std::mem::take(&mut self.addresses),
                }
            } else if has_addresses {
                Group {
                    name: self.group_comment.take(),
                    addresses: std::mem::take(&mut self.addresses),
                }
            } else if has_name {
                Group {
                    name: self.group_name.take(),
                    addresses: Vec::new(),
                }
            } else {
                return;
            });
    }
}

/// RFC 5322 section 4.4 direct source routes are transport history rather
/// than mailbox identity. Accept the obsolete route list on receipt and keep
/// only its terminal addr-spec. A route component is an `@domain`, including a
/// domain literal, with optional null relays between components. Anything that
/// does not meet that grammar remains literal rather than being reinterpreted
/// as another mailbox.
fn strip_obs_route<'x>(address: &'x str) -> Cow<'x, str> {
    let Some((route, terminal)) = split_obs_route(address) else {
        return address.into();
    };
    let terminal = terminal.trim();
    if !is_addr_spec(terminal) {
        return address.into();
    }
    if is_obs_route_list(route) {
        terminal.to_owned().into()
    } else {
        address.into()
    }
}

/// Locate the route delimiter without mistaking a colon in a domain literal,
/// quoted terminal, or CFWS comment. RFC 5322 permits comments around route
/// components, while a colon in those comments is not the route delimiter.
fn split_obs_route(address: &str) -> Option<(&str, &str)> {
    let mut literal = false;
    let mut quote = false;
    let mut escaped = false;
    let mut comment_depth = 0usize;
    let mut delimiter = None;
    for (index, byte) in address.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if comment_depth > 0 {
            match byte {
                b'\\' => escaped = true,
                b'(' => comment_depth += 1,
                b')' => comment_depth = comment_depth.checked_sub(1)?,
                _ => {}
            }
            continue;
        }
        if (literal || quote) && byte == b'\\' {
            escaped = true;
            continue;
        }
        if !literal && byte == b'"' {
            quote = !quote;
            continue;
        }
        if !quote && byte == b'[' {
            literal = true;
            continue;
        }
        if !quote && literal && byte == b']' {
            literal = false;
            continue;
        }
        if !quote && !literal && byte == b'(' {
            comment_depth = 1;
            continue;
        }
        if !quote && !literal && byte == b':' && delimiter.replace(index).is_some() {
            return None;
        }
    }
    (!literal && !quote && comment_depth == 0 && !escaped).then_some(())?;
    let delimiter = delimiter?;
    Some((&address[..delimiter], &address[delimiter + 1..]))
}

fn is_addr_spec(value: &str) -> bool {
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    is_dot_atom(local) && is_domain(domain)
}

fn is_obs_route_list(route: &str) -> bool {
    let bytes = route.as_bytes();
    let mut index = 0usize;
    let mut has_route = false;
    loop {
        let Some(()) = skip_cfws(bytes, &mut index) else {
            return false;
        };
        if index == bytes.len() {
            return has_route;
        }
        if bytes[index] == b',' {
            index += 1;
            continue;
        }
        if bytes[index] != b'@' {
            return false;
        }
        index += 1;
        let domain_start = index;
        if bytes.get(index) == Some(&b'[') {
            let Some(end) = consume_domain_literal(bytes, index) else {
                return false;
            };
            index = end;
        } else {
            while bytes
                .get(index)
                .is_some_and(|byte| is_atext(*byte) || *byte == b'.')
            {
                index += 1;
            }
        }
        if !is_domain(std::str::from_utf8(&bytes[domain_start..index]).unwrap_or_default()) {
            return false;
        }
        has_route = true;
        let Some(()) = skip_cfws(bytes, &mut index) else {
            return false;
        };
        if index == bytes.len() {
            return true;
        }
        if bytes[index] != b',' {
            return false;
        }
        index += 1;
    }
}

fn skip_cfws(bytes: &[u8], index: &mut usize) -> Option<()> {
    loop {
        loop {
            match bytes.get(*index) {
                Some(b' ' | b'\t') => *index += 1,
                // RFC 5322 folding whitespace is CRLF followed by WSP. A bare
                // CR/LF or an unfolded CRLF must not let a malformed route be
                // normalized into a terminal mailbox.
                Some(b'\r')
                    if matches!(bytes.get(*index + 1), Some(b'\n'))
                        && matches!(bytes.get(*index + 2), Some(b' ' | b'\t')) =>
                {
                    *index += 2;
                }
                Some(b'\r' | b'\n') => return None,
                _ => break,
            }
        }
        if bytes.get(*index) != Some(&b'(') {
            return Some(());
        }
        let mut depth = 1usize;
        let mut escaped = false;
        *index += 1;
        while depth > 0 {
            let byte = *bytes.get(*index)?;
            if escaped {
                // obs-quoted-pair accepts printable US-ASCII or horizontal
                // whitespace, but never a physical line break or another
                // arbitrary control byte.
                if byte != b'\t' && !(b' '..=b'~').contains(&byte) {
                    return None;
                }
                *index += 1;
                escaped = false;
                continue;
            }
            // Physical line breaks remain folding whitespace even while
            // scanning a comment. Do not let a bare break hide inside a route
            // comment and make an invalid route look valid.
            if byte == b'\r' {
                if bytes.get(*index + 1) != Some(&b'\n')
                    || !matches!(bytes.get(*index + 2), Some(b' ' | b'\t'))
                {
                    return None;
                }
                *index += 3;
                continue;
            }
            if byte == b'\n' {
                return None;
            }
            *index += 1;
            match byte {
                b'\\' => escaped = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                b' ' | b'\t' | 33..=39 | 42..=91 | 93..=126 => {}
                _ => return None,
            }
        }
        if escaped {
            return None;
        }
    }
}

fn consume_domain_literal(bytes: &[u8], mut index: usize) -> Option<usize> {
    debug_assert_eq!(bytes.get(index), Some(&b'['));
    let mut escaped = false;
    index += 1;
    while let Some(byte) = bytes.get(index) {
        index += 1;
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b']' {
            return Some(index);
        }
    }
    None
}

fn is_dot_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(is_atext))
}

fn is_domain(value: &str) -> bool {
    if let Some(literal) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return is_domain_literal(literal);
    }
    !value.is_empty()
        && value
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(is_atext))
}

fn is_domain_literal(value: &str) -> bool {
    let mut escaped = false;
    for byte in value.bytes() {
        if escaped {
            if !(byte == b' ' || byte == b'\t' || (33..=126).contains(&byte)) {
                return false;
            }
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if !(byte == b' '
            || byte == b'\t'
            || (33..=90).contains(&byte)
            || (94..=126).contains(&byte))
        {
            return false;
        }
    }
    !escaped
}

fn is_atext(byte: u8) -> bool {
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

impl<'x> MessageStream<'x> {
    pub fn parse_address(&mut self) -> HeaderValue<'x> {
        let mut parser = AddressParser {
            token_start: 0,
            token_end: 0,

            is_token_email: false,
            is_token_start: true,
            is_escaped: false,

            name_tokens: Vec::with_capacity(3),
            mail_tokens: Vec::with_capacity(3),
            comment_tokens: Vec::with_capacity(3),

            state: AddressState::Name,
            state_stack: Vec::with_capacity(5),

            addresses: Vec::new(),
            group_name: None,
            group_comment: None,
            raw_angle_address: None,
            angle_address_start: None,
            item_start: self.offset(),
            result: Vec::new(),
        };

        while let Some(ch) = self.next() {
            match ch {
                b'\n' => {
                    parser.add_token(self, false);
                    if parser.state == AddressState::Quote {
                        if self.peek_next_is_space() {
                            continue;
                        } else {
                            break;
                        }
                    }
                    if self.try_next_is_space() {
                        if !parser.is_token_start {
                            parser.is_token_start = true;
                        }
                        continue;
                    } else {
                        break;
                    }
                }
                b'\\' if parser.state != AddressState::Name && !parser.is_escaped => {
                    if parser.token_start > 0 {
                        if parser.state == AddressState::Quote {
                            parser.token_end = self.offset() - 1;
                        }
                        parser.add_token(self, false);
                    }
                    parser.is_escaped = true;
                    continue;
                }
                b',' if parser.state == AddressState::Name => {
                    parser.add_token(self, false);
                    parser.add_address(Some(self.bytes(parser.item_start..self.offset() - 1)));
                    parser.item_start = self.offset();
                    continue;
                }
                b'<' if parser.state == AddressState::Name => {
                    parser.is_token_email = false;
                    parser.add_token(self, false);
                    parser.state_stack.push(AddressState::Name);
                    parser.state = AddressState::Address;
                    parser.angle_address_start = Some(self.offset());
                    continue;
                }
                b'>' if parser.state == AddressState::Address => {
                    parser.add_token(self, false);
                    parser.raw_angle_address = parser
                        .angle_address_start
                        .take()
                        .map(|start| String::from_utf8_lossy(self.bytes(start..self.offset() - 1)));
                    parser.state = parser.state_stack.pop().unwrap();
                    continue;
                }
                b'"' if !parser.is_escaped => match parser.state {
                    AddressState::Name => {
                        parser.state_stack.push(AddressState::Name);
                        parser.state = AddressState::Quote;
                        parser.add_token(self, false);
                        continue;
                    }
                    AddressState::Quote => {
                        parser.add_token(self, false);
                        parser.state = parser.state_stack.pop().unwrap();
                        continue;
                    }
                    _ => (),
                },
                b'@' if parser.state == AddressState::Name => {
                    parser.is_token_email = true;
                }
                // Local Dakia safety patch: RFC 2047 encoded words are display
                // syntax and must never be decoded inside an addr-spec. The
                // parser is already in Address state for angle addr-specs; a
                // bare local part remains in Name state until its `@`, so
                // check the decoded-word suffix for an addr-spec continuation.
                b'=' if parser.state != AddressState::Address
                    && parser.is_token_start
                    && !parser.is_escaped
                    && self.peek_char(b'?') =>
                {
                    self.checkpoint();
                    if let Some(token) = self.decode_rfc2047() {
                        if !encoded_word_continues_addr_spec(self)
                            && !(parser.state == AddressState::Quote
                                && encoded_word_closes_quoted_local_part(self))
                        {
                            let add_space = parser.state != AddressState::Quote; // Make borrow-checker happy
                            parser.add_token(self, add_space);
                            (if parser.state != AddressState::Comment {
                                &mut parser.name_tokens
                            } else {
                                &mut parser.comment_tokens
                            })
                            .push(token.into());
                            continue;
                        }
                    }
                    self.restore();
                }
                b' ' | b'\t' => {
                    if !parser.is_token_start {
                        parser.is_token_start = true;
                    }
                    if parser.is_escaped {
                        parser.is_escaped = false;
                    }
                    if parser.state == AddressState::Quote {
                        if parser.token_start == 0 {
                            parser.token_start = self.offset();
                            parser.token_end = parser.token_start;
                        } else {
                            parser.token_end = self.offset();
                        }
                    }
                    continue;
                }
                b'\r' => continue,
                b'(' if parser.state != AddressState::Quote && !parser.is_escaped => {
                    parser.state_stack.push(parser.state);
                    if parser.state != AddressState::Comment {
                        parser.add_token(self, false);
                        parser.state = AddressState::Comment;
                        continue;
                    }
                }
                b')' if parser.state == AddressState::Comment && !parser.is_escaped => {
                    let new_state = parser.state_stack.pop().unwrap();
                    if parser.state != new_state {
                        parser.add_token(self, false);
                        parser.state = new_state;
                        continue;
                    }
                }
                b':' if parser.state == AddressState::Name && !parser.is_escaped => {
                    parser.add_group();
                    parser.add_token(self, false);
                    parser.add_group_details();
                    parser.item_start = self.offset();
                    continue;
                }
                b';' if parser.state == AddressState::Name => {
                    parser.add_token(self, false);
                    parser.add_address(Some(self.bytes(parser.item_start..self.offset() - 1)));
                    parser.item_start = self.offset();
                    parser.add_group();
                    continue;
                }
                _ => (),
            }

            if parser.is_escaped {
                parser.is_escaped = false;
            }

            if parser.is_token_start {
                parser.is_token_start = false;
            }

            if parser.token_start == 0 {
                parser.token_start = self.offset();
                parser.token_end = parser.token_start;
            } else {
                parser.token_end = self.offset();
            }
        }

        parser.add_address(Some(self.bytes(parser.item_start..self.offset())));

        if parser.group_name.is_some() || !parser.result.is_empty() {
            parser.add_group();
            HeaderValue::Address(Address::Group(parser.result))
        } else if !parser.addresses.is_empty() {
            HeaderValue::Address(Address::List(parser.addresses))
        } else {
            HeaderValue::Empty
        }
    }
}

/// Leading and trailing comments describe the mailbox for display. Preserve
/// every top-level edge comment if an otherwise valid obsolete addr-spec needs
/// repair, without promoting separator comments into a display name.
fn outer_display_comment(raw: &[u8]) -> Option<String> {
    let raw = std::str::from_utf8(raw).ok()?.trim();
    let comments = top_level_comments(raw)?;
    let mut retain = Vec::new();

    let mut prefix_end = 0usize;
    for (index, (start, end, _)) in comments.iter().enumerate() {
        if raw[prefix_end..*start].trim().is_empty() {
            retain.push(index);
            prefix_end = *end;
        } else {
            break;
        }
    }

    let mut suffix_start = raw.len();
    for (index, (start, end, _)) in comments.iter().enumerate().rev() {
        if raw[*end..suffix_start].trim().is_empty() {
            retain.push(index);
            suffix_start = *start;
        } else {
            break;
        }
    }
    retain.sort_unstable();
    retain.dedup();
    (!retain.is_empty()).then(|| {
        retain
            .into_iter()
            .map(|index| comments[index].2.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn top_level_comments(raw: &str) -> Option<Vec<(usize, usize, String)>> {
    let bytes = raw.as_bytes();
    let mut comments = Vec::new();
    let mut index = 0usize;
    let mut quoted = false;
    let mut literal = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
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
                b'(' => {
                    let (end, text) = comment_text(raw, index)?;
                    comments.push((index, end, text));
                    index = end;
                    continue;
                }
                _ => {}
            }
        }
        index += 1;
    }
    Some(comments)
}

fn comment_text(raw: &str, start: usize) -> Option<(usize, String)> {
    let bytes = raw.as_bytes();
    (bytes.get(start) == Some(&b'(')).then_some(())?;
    let mut depth = 1usize;
    let mut index = start + 1;
    let mut text = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 1;
                text.push(*bytes.get(index)?);
            }
            b'(' => {
                depth += 1;
                text.push(b'(');
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((index + 1, String::from_utf8(text).ok()?.trim().to_owned()));
                }
                text.push(b')');
            }
            byte => text.push(byte),
        }
        index += 1;
    }
    None
}

/// RFC 5322 section 4.4 permits CFWS around the dots and `@` in an
/// obsolete bare addr-spec. The generic display-name state treats a quoted
/// local word as a name before it sees `@`, so validate the complete original
/// item before applying this small receive-only normalization. CFWS within an
/// atom, unbalanced syntax, display phrases, and angle mailboxes remain on the
/// ordinary parser path.
fn normalize_obs_bare_addr_spec(raw: &[u8]) -> ObsBareAddrSpec {
    // Angle mailboxes and groups always use the ordinary parser, even when a
    // display name has legacy non-UTF-8 bytes. Classify this before UTF-8
    // validation so a quoted display name cannot fence its safe addr-spec.
    if raw.is_empty() || has_top_level_group_or_angle(raw) {
        return ObsBareAddrSpec::NotApplicable;
    }
    let Ok(raw) = std::str::from_utf8(raw) else {
        return (raw.contains(&b'@')
            && raw
                .iter()
                .any(|byte| matches!(*byte, b'"' | b'[' | b']' | b'\\')))
        .then_some(ObsBareAddrSpec::InvalidCandidate)
        .unwrap_or(ObsBareAddrSpec::NotApplicable);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return ObsBareAddrSpec::NotApplicable;
    }

    let Some((compact, had_obs_syntax)) = remove_obs_cfws(raw) else {
        return has_invalid_obs_bare_cfws_candidate(raw)
            .then_some(ObsBareAddrSpec::InvalidCandidate)
            .unwrap_or(ObsBareAddrSpec::NotApplicable);
    };
    let has_quoted_or_literal_syntax = raw
        .bytes()
        .any(|byte| matches!(byte, b'"' | b'[' | b']' | b'\\'));
    if !had_obs_syntax && !has_quoted_or_literal_syntax {
        return ObsBareAddrSpec::NotApplicable;
    }
    let Some((local, domain)) = split_top_level_addr_spec(&compact) else {
        return ObsBareAddrSpec::InvalidCandidate;
    };
    let (Some(local), Some(domain)) = (
        normalize_obs_local_part(local),
        normalize_obs_domain(domain),
    ) else {
        return ObsBareAddrSpec::InvalidCandidate;
    };
    ObsBareAddrSpec::Normalized(format!("{local}@{domain}"))
}

/// Angle brackets and a group colon select the ordinary address parser only
/// when they occur outside quoted local parts, domain literals, and comments.
/// Looking for the raw byte would let an invalid bare candidate bypass the
/// fail-closed path simply by placing a colon in data text.
fn has_top_level_group_or_angle(raw: &[u8]) -> bool {
    let mut quote = false;
    let mut literal = false;
    let mut escaped = false;
    let mut comment_depth = 0usize;
    let mut angle_start = None;

    for (index, byte) in raw.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if comment_depth > 0 {
            match *byte {
                b'\\' => escaped = true,
                b'(' => comment_depth += 1,
                b')' => comment_depth = comment_depth.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        if (quote || literal) && *byte == b'\\' {
            escaped = true;
            continue;
        }
        if !literal && *byte == b'"' {
            quote = !quote;
            continue;
        }
        if !quote && *byte == b'[' {
            literal = true;
            continue;
        }
        if !quote && literal && *byte == b']' {
            literal = false;
            continue;
        }
        if !quote && !literal && *byte == b'(' {
            comment_depth = 1;
            continue;
        }
        if !quote && !literal {
            match *byte {
                b':' if angle_start.is_none() => return true,
                b'<' if angle_start.is_none() => angle_start = Some(index),
                b'>' => {
                    if let Some(start) = angle_start.take()
                        && is_valid_angle_mailbox(&raw[start + 1..index])
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// An angle pair is ordinary framing only when its enclosed value has a
/// complete direct addr-spec terminal or a valid obsolete source route plus
/// that terminal. Empty or arbitrary brackets must not let a malformed bare
/// candidate fall through to token recovery.
fn is_valid_angle_mailbox(value: &[u8]) -> bool {
    let Ok(value) = std::str::from_utf8(value) else {
        return false;
    };
    let value = value.trim();
    if let Some((route, terminal)) = split_obs_route(value) {
        let Some((terminal, _)) = remove_obs_cfws(terminal.trim()) else {
            return false;
        };
        return is_obs_route_list(route) && is_valid_angle_terminal(&terminal);
    }
    // Direct terminals allow CFWS only at their dot and @ separators.
    let Some((value, _)) = remove_obs_cfws(value) else {
        return false;
    };
    is_valid_angle_terminal(&value)
}

/// Use the same local-part and domain validation as obsolete bare-address
/// repair, without changing the original angle representation. This accepts
/// quoted local parts and domain literals while rejecting partial terminals
/// such as `@`, `x@`, or `@y`.
fn is_valid_angle_terminal(value: &str) -> bool {
    let Some((local, domain)) = split_top_level_addr_spec(value) else {
        return false;
    };
    normalize_obs_local_part(local).is_some() && normalize_obs_domain(domain).is_some()
}

/// Split a bare addr-spec at its sole top-level `@`. RFC 5322 permits `@`
/// in quoted local parts and in domain literals, so a rightmost-byte split can
/// fence a valid mailbox as malformed.
fn split_top_level_addr_spec(value: &str) -> Option<(&str, &str)> {
    let bytes = value.as_bytes();
    let mut quote = false;
    let mut literal = false;
    let mut escaped = false;
    let mut separator = None;

    for (index, byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if (quote || literal) && *byte == b'\\' {
            escaped = true;
            continue;
        }
        if !literal && *byte == b'"' {
            quote = !quote;
            continue;
        }
        if !quote && *byte == b'[' {
            literal = true;
            continue;
        }
        if !quote && literal && *byte == b']' {
            literal = false;
            continue;
        }
        if !quote && !literal && *byte == b'@' {
            separator.replace(index).is_none().then_some(())?;
        }
    }

    (!quote && !literal && !escaped).then_some(())?;
    let separator = separator?;
    let local = value.get(..separator)?;
    let domain = value.get(separator + 1..)?;
    (!local.is_empty() && !domain.is_empty()).then_some((local, domain))
}

/// Detect the narrow malformed-obsolete form that would otherwise allow the
/// legacy token parser to discard a local-part prefix and return a different
/// mailbox. Ordinary phrases and edge comments remain on the legacy path.
fn has_invalid_obs_bare_cfws_candidate(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if !bytes.contains(&b'@') {
        return false;
    }
    if bytes
        .iter()
        .any(|byte| matches!(*byte, b'"' | b'[' | b']' | b'\\'))
    {
        return true;
    }

    let mut index = 0usize;
    let mut quote = false;
    let mut literal = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if (quote || literal) && byte == b'\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if !literal && byte == b'"' {
            quote = !quote;
            index += 1;
            continue;
        }
        if !quote && byte == b'[' {
            literal = true;
            index += 1;
            continue;
        }
        if !quote && literal && byte == b']' {
            literal = false;
            index += 1;
            continue;
        }
        if !quote && !literal && byte == b'(' {
            let comment_start = index;
            let mut depth = 0usize;
            loop {
                let Some(current) = bytes.get(index) else {
                    return bytes
                        .get(comment_start.wrapping_sub(1))
                        .is_some_and(|previous| !previous.is_ascii_whitespace());
                };
                if *current == b'\\' {
                    index += 2;
                    continue;
                }
                if *current == b'(' {
                    depth += 1;
                } else if *current == b')' {
                    depth -= 1;
                    if depth == 0 {
                        index += 1;
                        break;
                    }
                }
                index += 1;
            }
            let previous = comment_start
                .checked_sub(1)
                .and_then(|index| bytes.get(index));
            let next = bytes.get(index);
            if let (Some(previous), Some(next)) = (previous, next) {
                if !previous.is_ascii_whitespace()
                    && !next.is_ascii_whitespace()
                    && !matches!((*previous, *next), (b'.' | b'@', _) | (_, b'.' | b'@'))
                {
                    return true;
                }
            }
            continue;
        }
        index += 1;
    }
    false
}

/// Remove only syntactically valid CFWS around addr-spec separators. Comments
/// or whitespace embedded in an atom are deliberately rejected rather than
/// silently joining bytes into a different mailbox identity.
fn remove_obs_cfws(raw: &str) -> Option<(String, bool)> {
    let bytes = raw.as_bytes();
    let mut compact = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    let mut quote = false;
    let mut literal = false;
    let mut escaped = false;
    let mut had_obs_syntax = false;
    let mut previous = None;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            compact.push(byte);
            escaped = false;
            previous = Some(byte);
            index += 1;
            continue;
        }
        if (quote || literal) && byte == b'\\' {
            compact.push(b'\\');
            escaped = true;
            had_obs_syntax |= literal;
            previous = Some(byte);
            index += 1;
            continue;
        }
        if !literal && byte == b'"' {
            quote = !quote;
            compact.push(b'"');
            previous = Some(byte);
            had_obs_syntax = true;
            index += 1;
            continue;
        }
        if !quote && byte == b'[' {
            literal = true;
            compact.push(b'[');
            previous = Some(byte);
            index += 1;
            continue;
        }
        if !quote && literal && byte == b']' {
            literal = false;
            compact.push(b']');
            previous = Some(byte);
            index += 1;
            continue;
        }
        if !quote && !literal && (byte.is_ascii_whitespace() || byte == b'(') {
            let run_start = index;
            let mut saw_comment = false;
            loop {
                while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                    index += 1;
                }
                if bytes.get(index) != Some(&b'(') {
                    break;
                }
                saw_comment = true;
                let mut depth = 0usize;
                loop {
                    let current = *bytes.get(index)?;
                    if current == b'\\' {
                        index += 2;
                        continue;
                    }
                    if current == b'(' {
                        depth += 1;
                    } else if current == b')' {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            index += 1;
                            break;
                        }
                    }
                    index += 1;
                }
            }
            let next = *bytes.get(index).unwrap_or(&0);
            let previous = previous.unwrap_or(0);
            if !(previous == 0
                || next == 0
                || matches!(previous, b'.' | b'@')
                || matches!(next, b'.' | b'@'))
            {
                return None;
            }
            // Ordinary whitespace around a dot has an established parser
            // representation. Only repair a bare item when comments mark the
            // separator or CFWS occurs at the `@` boundary, where the generic
            // display-name state would otherwise lose local-part tokens.
            had_obs_syntax |= (saw_comment
                && (matches!(previous, b'.' | b'@') || matches!(next, b'.' | b'@')))
                || previous == b'@'
                || next == b'@';
            if index == run_start {
                return None;
            }
            continue;
        }
        compact.push(byte);
        previous = Some(byte);
        index += 1;
    }

    (!quote && !literal && !escaped)
        .then(|| {
            String::from_utf8(compact)
                .ok()
                .map(|compact| (compact, had_obs_syntax))
        })
        .flatten()
}

fn normalize_obs_local_part(local: &str) -> Option<String> {
    let bytes = local.as_bytes();
    let mut index = 0usize;
    let mut words = Vec::<Vec<u8>>::new();
    let mut quoted = false;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            quoted = true;
            index += 1;
            let mut word = Vec::new();
            while index < bytes.len() && bytes[index] != b'"' {
                if bytes[index] == b'\\' {
                    index += 1;
                }
                let byte = *bytes.get(index)?;
                if byte.is_ascii_control() {
                    return None;
                }
                word.push(byte);
                index += 1;
            }
            (bytes.get(index) == Some(&b'"')).then_some(())?;
            String::from_utf8(word.clone()).ok()?;
            index += 1;
            words.push(word);
        } else {
            let start = index;
            while bytes.get(index).is_some_and(|byte| *byte != b'.') {
                index += 1;
            }
            let word = local.get(start..index)?;
            is_dot_atom(word).then_some(())?;
            words.push(word.as_bytes().to_vec());
        }
        if index == bytes.len() {
            break;
        }
        (bytes.get(index) == Some(&b'.')).then_some(())?;
        index += 1;
    }
    (!words.is_empty()).then_some(())?;
    let mut value = Vec::new();
    for (index, word) in words.into_iter().enumerate() {
        if index > 0 {
            value.push(b'.');
        }
        value.extend(word);
    }
    let value = String::from_utf8(value).ok()?;
    if quoted || !is_dot_atom(&value) {
        Some(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    } else {
        Some(value)
    }
}

fn normalize_obs_domain(domain: &str) -> Option<String> {
    if let Some(value) = domain
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        let mut semantic = Vec::with_capacity(value.len());
        let mut escaped = false;
        for byte in value.bytes() {
            if escaped {
                (!byte.is_ascii_control()).then_some(())?;
                semantic.push(byte);
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte.is_ascii_control() || byte == b'[' || byte == b']' {
                return None;
            } else {
                semantic.push(byte);
            }
        }
        (!escaped).then_some(())?;
        String::from_utf8(semantic.clone()).ok()?;
        let mut normalized = Vec::with_capacity(semantic.len() + 2);
        normalized.push(b'[');
        for byte in semantic {
            if matches!(byte, b'[' | b']' | b'\\') {
                normalized.push(b'\\');
            }
            normalized.push(byte);
        }
        normalized.push(b']');
        String::from_utf8(normalized).ok()
    } else {
        is_domain(domain).then_some(domain.to_owned())
    }
}

/// An encoded word followed by an addr-spec continuation is part of a bare
/// mailbox local part, not RFC 2047 display text. This uses the RFC 5322 atom
/// character set and also recognizes the narrow obsolete form where CFWS
/// separates that local part from its `@`. It deliberately does not scan past
/// CFWS followed by another word, which remains an ordinary display phrase.
fn encoded_word_continues_addr_spec(stream: &MessageStream<'_>) -> bool {
    stream.peek_bytes(stream.remaining()).is_some_and(|bytes| {
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'@' {
                return true;
            }
            if !matches!(
                *byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'!'
                    | b'#'
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
                    | b'.'
            ) {
                return cfws_followed_by_at(bytes, index);
            }
        }
        false
    })
}

/// A quoted encoded-looking token followed by the closing local-part quote and
/// an addr-spec continuation is still literal mailbox data. Keep it raw while
/// continuing to decode a quoted display name before an angle mailbox.
fn encoded_word_closes_quoted_local_part(stream: &MessageStream<'_>) -> bool {
    let bytes = stream.peek_bytes(stream.remaining()).unwrap_or_default();
    let mut index = 0usize;
    let mut escaped = false;
    while let Some(byte) = bytes.get(index) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return cfws_followed_by_at(bytes, index + 1) || bytes.get(index + 1) == Some(&b'@');
        }
        index += 1;
    }
    false
}

/// Returns true only for CFWS directly followed by `@`. This preserves a
/// receive-only obsolete local-part spelling without mistaking an encoded
/// display word followed by `real@example.test` for a mailbox local part.
fn cfws_followed_by_at(bytes: &[u8], mut index: usize) -> bool {
    let mut consumed_cfws = false;
    let mut comment_depth = 0usize;
    let mut escaped = false;

    while let Some(byte) = bytes.get(index) {
        if escaped {
            escaped = false;
            consumed_cfws = true;
            index += 1;
            continue;
        }
        if comment_depth > 0 {
            match *byte {
                b'\\' => escaped = true,
                b'(' => comment_depth += 1,
                b')' => comment_depth -= 1,
                _ => {}
            }
            consumed_cfws = true;
            index += 1;
            continue;
        }
        match *byte {
            b' ' | b'\t' | b'\r' | b'\n' => {
                consumed_cfws = true;
                index += 1;
            }
            b'\\' if comment_depth > 0 => {
                escaped = true;
                consumed_cfws = true;
                index += 1;
            }
            b'(' => {
                comment_depth += 1;
                consumed_cfws = true;
                index += 1;
            }
            b'@' if comment_depth == 0 => return consumed_cfws,
            _ => return false,
        }
    }

    false
}

fn concat_tokens<'x>(tokens: &mut Vec<Cow<'x, str>>) -> Cow<'x, str> {
    if tokens.len() == 1 {
        tokens.pop().unwrap()
    } else {
        let result = tokens.concat();
        tokens.clear();
        result.into()
    }
}

pub fn parse_address_local_part(addr: &str) -> Option<&str> {
    let addr = addr.as_bytes();
    let mut iter = addr.iter().enumerate();
    while let Some((pos, &ch)) = iter.next() {
        if ch == b'@' {
            return if pos > 0 && iter.next().is_some() {
                std::str::from_utf8(addr.get(..pos)?).ok()
            } else {
                None
            };
        } else if !ch.is_ascii() {
            return None;
        }
    }

    None
}

pub fn parse_address_domain(addr: &str) -> Option<&str> {
    let addr = addr.as_bytes();
    for (pos, &ch) in addr.iter().enumerate() {
        if ch == b'@' {
            return if pos > 0 && pos + 1 < addr.len() {
                std::str::from_utf8(addr.get(pos + 1..)?).ok()
            } else {
                None
            };
        } else if !ch.is_ascii() {
            return None;
        }
    }

    None
}

pub fn parse_address_user_part(addr: &str) -> Option<&str> {
    let addr = addr.as_bytes();

    let mut iter = addr.iter().enumerate();
    while let Some((pos, &ch)) = iter.next() {
        if ch == b'+' {
            if pos > 0 {
                while let Some((_, &ch)) = iter.next() {
                    if ch == b'@' && iter.next().is_some() {
                        return std::str::from_utf8(addr.get(..pos)?).ok();
                    }
                }
            }
            return None;
        } else if ch == b'@' {
            return if pos > 0 && iter.next().is_some() {
                std::str::from_utf8(addr.get(..pos)?).ok()
            } else {
                None
            };
        } else if !ch.is_ascii() {
            return None;
        }
    }

    None
}

pub fn parse_address_detail_part(addr: &str) -> Option<&str> {
    let addr = addr.as_bytes();
    let mut plus_pos = usize::MAX;

    let mut iter = addr.iter().enumerate();
    while let Some((pos, &ch)) = iter.next() {
        if ch == b'+' {
            plus_pos = pos + 1;
        } else if ch == b'@' {
            if plus_pos != usize::MAX && iter.next().is_some() {
                return std::str::from_utf8(addr.get(plus_pos..pos)?).ok();
            } else {
                return None;
            }
        } else if !ch.is_ascii() {
            return None;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::strip_obs_route;
    use crate::parsers::{fields::load_tests, MessageStream};

    #[test]
    fn parse_addresses() {
        for test in load_tests("address.json") {
            assert_eq!(
                MessageStream::new(test.header.as_bytes())
                    .parse_address()
                    .unwrap_address(),
                test.expected,
                "failed for {:?}",
                test.header
            );
        }
    }

    #[test]
    fn rfc5322_obs_route_keeps_only_terminal_mailbox() {
        for (input, expected) in [
            (
                "<@relay-a.example,@relay-b.example:sender@example.test>\r\n",
                "sender@example.test",
            ),
            (
                "<,@relay-a.example,,@relay-b.example:reader@example.test>\r\n",
                "reader@example.test",
            ),
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            assert_eq!(
                address.first().and_then(|value| value.address()),
                Some(expected)
            );
        }
    }

    #[test]
    fn rfc5322_obs_route_requires_valid_route_domains_and_terminal_addr_spec() {
        for (input, expected) in [
            (
                "@relay.example,@[IPv6:2001:db8::1]:sender@[IPv6:2001:db8::2]",
                "sender@[IPv6:2001:db8::2]",
            ),
            (
                "@relay..example:sender@example.test",
                "@relay..example:sender@example.test",
            ),
            (
                "relay.example:sender@example.test",
                "relay.example:sender@example.test",
            ),
            (
                "@relay.example:quoted local@example.test",
                "@relay.example:quoted local@example.test",
            ),
        ] {
            assert_eq!(strip_obs_route(input), expected, "{input}");
        }
    }

    #[test]
    fn rfc5322_obs_route_cfws_requires_a_valid_folding_boundary() {
        for input in [
            "@relay.example\r:sender@example.test",
            "@relay.example\n:sender@example.test",
            "@relay.example\r\n:sender@example.test",
            "@relay.example(comment\rmore):sender@example.test",
            "@relay.example(comment\nmore):sender@example.test",
            "@relay.example(comment\r\nmore):sender@example.test",
            "@relay.example(comment\x01bad):sender@example.test",
            "@relay.example(comment\\\r\n more):sender@example.test",
        ] {
            assert_eq!(strip_obs_route(input), input, "{input:?}");
        }
        for (input, expected) in [
            (
                "@relay.example\r\n \t:sender@example.test",
                "sender@example.test",
            ),
            (
                "@relay.example(comment: detail):sender@example.test",
                "sender@example.test",
            ),
            (
                "@relay.example(comment\r\n detail):sender@example.test",
                "sender@example.test",
            ),
            (
                "@relay.example(comment (nested)\\) detail):sender@example.test",
                "sender@example.test",
            ),
            (
                "@[IPv6:2001:db8::1]:sender@example.test",
                "sender@example.test",
            ),
        ] {
            assert_eq!(strip_obs_route(input), expected, "{input:?}");
        }
    }

    #[test]
    fn malformed_obs_route_line_breaks_cannot_yield_the_terminal_mailbox() {
        for route in [
            "@relay.example\r:sender@example.test",
            "@relay.example\n:sender@example.test",
            "@relay.example\r\n:sender@example.test",
            "@relay.example(comment\rmore):sender@example.test",
            "@relay.example(comment\nmore):sender@example.test",
            "@relay.example(comment\r\nmore):sender@example.test",
            "@relay.example(comment\\\r\n more):sender@example.test",
        ] {
            let raw = format!("From: <{route}>\r\n\r\n");
            let message = crate::MessageParser::new()
                .with_minimal_headers()
                .parse(raw.as_bytes())
                .expect("recoverable malformed route header");
            let address = message
                .header(crate::HeaderName::From)
                .and_then(crate::HeaderValue::as_address)
                .and_then(|addresses| addresses.first())
                .and_then(|address| address.address());
            assert_ne!(address, Some("sender@example.test"), "{route:?}");
        }
    }

    #[test]
    fn obs_route_validation_uses_the_original_angle_span() {
        for (input, expected) in [
            (
                "<@relay.example:first.last@example.test>\r\n",
                Some("first.last@example.test"),
            ),
            (
                "<@relay.example:sender@[foo(bar)]>\r\n",
                Some("sender@[foo(bar)]"),
            ),
            (
                "<@relay.example:sender@[foo\\]bar]>\r\n",
                Some("sender@[foo\\]bar]"),
            ),
            // Invalid source-route CFWS must never be reinterpreted as a
            // different actionable mailbox.
            ("<@rel(comment)ay.example:sender@example.test>\r\n", None),
            (
                "<@relay.example:\"left word\"@example.test>\r\n",
                Some("@relay.example:\"left word\"@example.test"),
            ),
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            assert_eq!(
                address.first().and_then(|value| value.address()),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn rfc5322_obs_bare_addr_specs_preserve_mailbox_identity() {
        for (input, expected) in [
            (
                "\"=?UTF-8?Q?attacker?=\" (note) @example.test\r\n",
                "\"=?UTF-8?Q?attacker?=\"@example.test",
            ),
            (
                "sender (a) . part @ example (b) . test\r\n",
                "sender.part@example.test",
            ),
            (
                "\"left word\" . part @ example . test\r\n",
                "\"left word.part\"@example.test",
            ),
            ("sender@[192\\.0.2.10]\r\n", "sender@[192.0.2.10]"),
            ("sender@[foo\\]bar]\r\n", "sender@[foo\\]bar]"),
            ("sender@[foo\\[bar]\r\n", "sender@[foo\\[bar]"),
            ("sender@[foo\\\\bar]\r\n", "sender@[foo\\\\bar]"),
            (
                "sender . part \t@\r\n example . test\r\n",
                "sender.part@example.test",
            ),
            ("\"é\" (note) @example.test\r\n", "\"é\"@example.test"),
            (
                "\"é\" . part @ example . test\r\n",
                "\"é.part\"@example.test",
            ),
            ("sender@[café\\.test]\r\n", "sender@[café.test]"),
            ("sender@[foo@bar]\r\n", "sender@[foo@bar]"),
            ("sender@[foo\\@bar]\r\n", "sender@[foo@bar]"),
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            assert_eq!(
                address.first().and_then(|value| value.address()),
                Some(expected),
                "{input}",
            );
        }
    }

    #[test]
    fn obs_bare_addr_spec_keeps_leading_or_trailing_display_comments() {
        for (input, expected_name) in [
            ("(Alice) sender@example.test\r\n", Some("Alice")),
            ("sender@example.test (Alice)\r\n", Some("Alice")),
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            let first = address.first().expect("one mailbox");
            assert_eq!(first.address(), Some("sender@example.test"), "{input}");
            assert_eq!(first.name(), expected_name, "{input}");
        }
    }

    #[test]
    fn obs_bare_addr_spec_preserves_outer_display_comments_when_repaired() {
        for input in [
            "(Alice) sender @ example.test\r\n",
            "sender @ example.test (Alice)\r\n",
            "(Alice) sender (separator) @ example.test\r\n",
            "sender (separator) @ example.test (Alice)\r\n",
            "(Alice) \"left word\" . part @ example . test\r\n",
            "\"left (\" @ example.test (Alice)\r\n",
            "sender @ [foo(bar] (Alice)\r\n",
            "(Alice) (Team) sender @ example.test\r\n",
            "sender @ example.test (Alice) (Team)\r\n",
            "(Alice) sender @ example.test (Team)\r\n",
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            let first = address.first().expect("one mailbox");
            let expected_address = if input.contains("left word") {
                "\"left word.part\"@example.test"
            } else if input.contains("left (") {
                "\"left (\"@example.test"
            } else if input.contains("foo(bar") {
                "sender@[foo(bar]"
            } else {
                "sender@example.test"
            };
            assert_eq!(first.address(), Some(expected_address), "{input}");
            let expected_name = input
                .contains("Team")
                .then_some("Alice Team")
                .unwrap_or("Alice");
            assert_eq!(first.name(), Some(expected_name), "{input}");
        }
    }

    #[test]
    fn configured_parser_uses_value_offset_for_obs_bare_addr_specs() {
        for (value, expected_address, expected_name) in [
            (
                "\"=?UTF-8?Q?attacker?=\" (note) @example.test",
                "\"=?UTF-8?Q?attacker?=\"@example.test",
                None,
            ),
            (
                "sender (a) . part @ example (b) . test",
                "sender.part@example.test",
                None,
            ),
            (
                "\"left word\" . part @ example . test",
                "\"left word.part\"@example.test",
                None,
            ),
            ("sender@[192\\.0.2.10]", "sender@[192.0.2.10]", None),
            ("sender@[foo@bar]", "sender@[foo@bar]", None),
            ("sender@[foo\\@bar]", "sender@[foo@bar]", None),
            (
                "(Alice) sender @ example.test",
                "sender@example.test",
                Some("Alice"),
            ),
        ] {
            let raw = format!("From: {value}\r\n\r\n");
            let message = crate::MessageParser::new()
                .with_minimal_headers()
                .parse(raw.as_bytes())
                .expect("complete header");
            let first = message
                .header(crate::HeaderName::From)
                .and_then(crate::HeaderValue::as_address)
                .and_then(|addresses| addresses.first())
                .expect("one address");
            assert_eq!(first.address(), Some(expected_address), "{value}");
            assert_eq!(first.name(), expected_name, "{value}");
        }
    }

    #[test]
    fn legacy_non_utf8_display_name_keeps_angle_mailbox() {
        let message = crate::MessageParser::new()
            .parse(b"To: \"J\xfcrgen Schm\xfcrgen\" <schmuergen@example.test>\r\n\r\n" as &[u8])
            .expect("complete message with legacy display encoding");
        let first = message
            .header(crate::HeaderName::To)
            .and_then(crate::HeaderValue::as_address)
            .and_then(|addresses| addresses.first())
            .expect("angle mailbox");
        assert_eq!(first.address(), Some("schmuergen@example.test"));
    }

    #[test]
    fn malformed_non_utf8_bare_candidates_cannot_bypass_the_fail_closed_path() {
        for raw in [
            b"From: \"\xff:foo\" (sep) @example.test\r\n\r\n" as &[u8],
            b"From: sender@[foo:\xff]\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test >\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <\r\n\r\n" as &[u8],
            b"From: < \"\xfffoo\" (sep) @example.test\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <>\r\n\r\n" as &[u8],
            b"From: <> \"\xfffoo\" (sep) @example.test\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <garbage>\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <@>\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <x@>\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <@y>\r\n\r\n" as &[u8],
            b"From: \"\xfffoo\" (sep) @example.test <@relay:garbage>\r\n\r\n" as &[u8],
            b"From: \"foo\" (sep) @example.test <>\r\n\r\n" as &[u8],
            b"From: \"foo\" (sep) @example.test >\r\n\r\n" as &[u8],
            b"From: sen(comment)der@example.test <>\r\n\r\n" as &[u8],
        ] {
            let message = crate::MessageParser::new()
                .with_minimal_headers()
                .with_message_ids()
                .parse(raw)
                .expect("recoverable malformed header");
            let first = message
                .header(crate::HeaderName::From)
                .and_then(crate::HeaderValue::as_address)
                .and_then(|addresses| addresses.first())
                .expect("opaque malformed mailbox");
            assert_eq!(first.address(), None, "{raw:?}");
        }
    }

    #[test]
    fn valid_angle_terminals_remain_on_the_ordinary_parser_path() {
        for (raw, expected) in [
            (
                b"From: \"J\xffrgen\" <safe@example.test>\r\n\r\n" as &[u8],
                "safe@example.test",
            ),
            (
                b"From: <@relay.example:sender@example.test>\r\n\r\n" as &[u8],
                "sender@example.test",
            ),
            (
                b"From: Name <@relay.example(comment):sender@example.test>\r\n\r\n" as &[u8],
                "sender@example.test",
            ),
            (
                b"From: Name <@relay.example(comment: detail):sender@example.test>\r\n\r\n"
                    as &[u8],
                "sender@example.test",
            ),
            (
                b"From: Pete <pete(comment)@silly.test(host)>\r\n\r\n" as &[u8],
                "pete@silly.test",
            ),
        ] {
            let message = crate::MessageParser::new()
                .with_minimal_headers()
                .with_message_ids()
                .parse(raw)
                .expect("recoverable address header");
            let first = message
                .header(crate::HeaderName::From)
                .and_then(crate::HeaderValue::as_address)
                .and_then(|addresses| addresses.first())
                .expect("one mailbox");
            assert_eq!(first.address(), Some(expected), "{raw:?}");
        }
    }

    #[test]
    fn obs_bare_addr_spec_rejects_unsafe_or_unterminated_normalization() {
        for input in [
            b"sender@[foo\\" as &[u8],
            b"sender@[foo\x01]",
            b"sender@[\xff]",
        ] {
            assert_eq!(
                super::normalize_obs_bare_addr_spec(input),
                super::ObsBareAddrSpec::InvalidCandidate,
                "{input:?}"
            );
        }
    }

    #[test]
    fn obs_bare_addr_spec_does_not_rewrite_cfws_inside_an_atom() {
        let input = "sen(comment)der@example.test\r\n";
        let address = MessageStream::new(input.as_bytes())
            .parse_address()
            .unwrap_address();
        let first = address.first().expect("opaque malformed mailbox");
        assert_eq!(first.address(), None);
        assert_eq!(first.name(), Some("sen(comment)der@example.test"));

        let raw = format!("From: {input}\r\n");
        let message = crate::MessageParser::new()
            .with_minimal_headers()
            .with_message_ids()
            .parse(raw.as_bytes())
            .expect("complete malformed header recovery");
        let first = message
            .header(crate::HeaderName::From)
            .and_then(crate::HeaderValue::as_address)
            .and_then(|addresses| addresses.first())
            .expect("opaque malformed mailbox in configured parser");
        assert_eq!(first.address(), None);
        assert_eq!(first.name(), Some("sen(comment)der@example.test"));
    }

    #[test]
    fn rfc2047_words_never_rewrite_addr_specs() {
        for (input, expected_name, expected_address) in [
            (
                "Sender <=?UTF-8?Q?attacker?=@example.test>\r\n",
                Some("Sender"),
                "=?UTF-8?Q?attacker?=@example.test",
            ),
            (
                "=?UTF-8?Q?attacker?=@example.test\r\n",
                None,
                "=?UTF-8?Q?attacker?=@example.test",
            ),
            (
                "=?UTF-8?Q?Display_Name?= <safe@example.test>\r\n",
                Some("Display Name"),
                "safe@example.test",
            ),
        ] {
            let address = MessageStream::new(input.as_bytes())
                .parse_address()
                .unwrap_address();
            let first = address.first().expect("one mailbox");
            assert_eq!(first.name(), expected_name, "{input}");
            assert_eq!(first.address(), Some(expected_address), "{input}");
        }
    }
}
