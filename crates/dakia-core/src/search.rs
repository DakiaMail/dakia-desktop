//! The provider-independent search language.
//!
//! This module deliberately only parses the user-facing language.  Storage and
//! provider adapters consume this typed representation rather than attempting
//! to reinterpret a raw search string.

use chrono::NaiveDate;
use std::fmt;

/// Maximum number of UTF-8 bytes accepted in one search query.
pub const MAX_QUERY_BYTES: usize = 4 * 1024;
/// Maximum number of lexical tokens accepted in one search query.
pub const MAX_QUERY_TOKENS: usize = 256;
/// Maximum parenthesis nesting accepted in one search query.
pub const MAX_QUERY_DEPTH: usize = 16;
/// Maximum leaf predicates accepted in one search query.
pub const MAX_QUERY_OPERANDS: usize = 128;
/// Maximum UTF-8 bytes in a single literal.
pub const MAX_QUERY_LITERAL_BYTES: usize = 1024;

/// A successfully parsed Fastmail-style search expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchExpression {
    pub root: SearchNode,
}

impl SearchExpression {
    pub fn match_all() -> Self {
        Self {
            root: SearchNode::MatchAll,
        }
    }
}

/// A boolean search expression. `And` and `Or` are flattened by the parser to
/// make their evaluation deterministic and simple for every backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchNode {
    MatchAll,
    Term(SearchTerm),
    And(Vec<SearchNode>),
    Or(Vec<SearchNode>),
    Not(Box<SearchNode>),
}

/// A single searchable predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchTerm {
    Text(SearchText),
    Field {
        field: SearchField,
        value: SearchText,
    },
    Folder(FolderScope),
    Date {
        comparison: DateComparison,
        value: SearchDate,
    },
    Attachment(AttachmentPredicate),
    Filename(SearchText),
    FileType(FileType),
    State(MessageState),
}

/// Text supplied by the user. Normalization and matching are intentionally
/// deferred to the canonical evaluator, so the original Unicode is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchText {
    pub value: String,
    pub quoted: bool,
    pub prefix: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    From,
    To,
    ToNotCc,
    Cc,
    Bcc,
    With,
    Subject,
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderScope {
    /// A single mailbox path.
    Exact(String),
    /// A mailbox and all of its descendants (`in:Projects/*`).
    Descendants(String),
    /// All mailboxes, including Spam and Trash (`in:*`).
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateComparison {
    On,
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchDate {
    Absolute(NaiveDate),
    Relative { amount: u32, unit: RelativeDateUnit },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeDateUnit {
    Days,
    Weeks,
    Months,
    Years,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentPredicate {
    HasAttachment,
    HasNoAttachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Pdf,
    Document,
    Spreadsheet,
    Presentation,
    Image,
    Audio,
    Video,
    Archive,
    Calendar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    Read,
    Unread,
    Flagged,
    Unflagged,
    Replied,
    Unreplied,
    Draft,
    Undraft,
}

/// A parser error with a UTF-8 byte position suitable for highlighting the
/// offending portion of the original query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParseError {
    pub position: usize,
    pub kind: SearchParseErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchParseErrorKind {
    ControlCharacter,
    UnterminatedQuote,
    InvalidEscape,
    UnexpectedToken { token: String },
    MissingOperand,
    MissingLiteral { operator: String },
    UnknownOperator { operator: String },
    UnsupportedOperator { operator: String },
    InvalidLiteral { value: String, reason: &'static str },
    InvalidDate { value: String },
    LimitExceeded { limit: SearchLimit, maximum: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchLimit {
    QueryBytes,
    Tokens,
    Depth,
    Operands,
    LiteralBytes,
}

impl fmt::Display for SearchParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "search query error at byte {}: ", self.position)?;
        match &self.kind {
            SearchParseErrorKind::ControlCharacter => {
                write!(formatter, "control characters are not allowed")
            }
            SearchParseErrorKind::UnterminatedQuote => {
                write!(formatter, "unterminated quoted text")
            }
            SearchParseErrorKind::InvalidEscape => write!(formatter, "invalid escape sequence"),
            SearchParseErrorKind::UnexpectedToken { token } => {
                write!(formatter, "unexpected token {token:?}")
            }
            SearchParseErrorKind::MissingOperand => write!(formatter, "missing operand"),
            SearchParseErrorKind::MissingLiteral { operator } => {
                write!(formatter, "missing value for {operator}:")
            }
            SearchParseErrorKind::UnknownOperator { operator } => {
                write!(formatter, "unknown operator {operator}:")
            }
            SearchParseErrorKind::UnsupportedOperator { operator } => {
                write!(formatter, "{operator}: is not supported yet")
            }
            SearchParseErrorKind::InvalidLiteral { value, reason } => {
                write!(formatter, "invalid literal {value:?}: {reason}")
            }
            SearchParseErrorKind::InvalidDate { value } => {
                write!(formatter, "invalid date {value:?}")
            }
            SearchParseErrorKind::LimitExceeded { limit, maximum } => {
                write!(formatter, "{limit:?} limit of {maximum} exceeded")
            }
        }
    }
}

impl std::error::Error for SearchParseError {}

/// Parse a user-entered search query into the provider-independent AST.
pub fn parse_search_query(input: &str) -> Result<SearchExpression, SearchParseError> {
    if input.len() > MAX_QUERY_BYTES {
        return Err(error(
            // Point at the first byte outside the accepted query, which lets
            // callers highlight the actual limit boundary instead of the
            // unrelated beginning of the query.
            MAX_QUERY_BYTES,
            SearchParseErrorKind::LimitExceeded {
                limit: SearchLimit::QueryBytes,
                maximum: MAX_QUERY_BYTES,
            },
        ));
    }

    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Ok(SearchExpression::match_all());
    }

    let mut parser = Parser {
        tokens,
        cursor: 0,
        operands: 0,
        depth: 0,
    };
    let root = parser.parse_or()?;
    if let Some(token) = parser.peek() {
        return Err(error(
            token.start,
            SearchParseErrorKind::UnexpectedToken {
                token: token.display(),
            },
        ));
    }
    Ok(SearchExpression { root })
}

/// Concise alias for callers which do not need to distinguish search parsers.
pub fn parse(input: &str) -> Result<SearchExpression, SearchParseError> {
    parse_search_query(input)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

impl Token {
    fn display(&self) -> String {
        match &self.kind {
            TokenKind::Word(value) | TokenKind::Quoted(value) => value.clone(),
            TokenKind::LParen => "(".into(),
            TokenKind::RParen => ")".into(),
            TokenKind::Plus => "+".into(),
            TokenKind::Minus => "-".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Word(String),
    Quoted(String),
    LParen,
    RParen,
    Plus,
    Minus,
}

fn lex(input: &str) -> Result<Vec<Token>, SearchParseError> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < input.len() {
        let character = input[index..]
            .chars()
            .next()
            .expect("valid character boundary");
        if character.is_control() {
            return Err(error(index, SearchParseErrorKind::ControlCharacter));
        }
        if character.is_whitespace() {
            index += character.len_utf8();
            continue;
        }

        let start = index;
        let kind = match character {
            '(' => {
                index += 1;
                TokenKind::LParen
            }
            ')' => {
                index += 1;
                TokenKind::RParen
            }
            '+' => {
                index += 1;
                TokenKind::Plus
            }
            '-' => {
                index += 1;
                TokenKind::Minus
            }
            '"' | '\'' => TokenKind::Quoted(lex_quoted(input, &mut index, start, character)?),
            _ => {
                let mut value = String::new();
                while index < input.len() {
                    let word = input[index..]
                        .chars()
                        .next()
                        .expect("valid character boundary");
                    if word.is_control() {
                        return Err(error(index, SearchParseErrorKind::ControlCharacter));
                    }
                    if word.is_whitespace()
                        || matches!(word, '(' | ')' | '"')
                        // Apostrophes are ordinary punctuation in an unquoted
                        // word (`don't`). They begin a quoted field value only
                        // immediately after its operator (`subject:'...'`).
                        || (word == '\'' && (value.is_empty() || value.ends_with(':')))
                    {
                        break;
                    }
                    // A leading unary +/- is lexical syntax; an address such
                    // as `alex+work@example.test` remains one literal.
                    if value.is_empty() && matches!(word, '+' | '-') {
                        break;
                    }
                    value.push(word);
                    index += word.len_utf8();
                }
                if value.is_empty() {
                    // The only way here is an operator handled above, but
                    // retaining this guard avoids a non-advancing lexer.
                    return Err(error(
                        start,
                        SearchParseErrorKind::UnexpectedToken {
                            token: character.to_string(),
                        },
                    ));
                }
                validate_literal(&value, start)?;
                TokenKind::Word(value)
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
        if tokens.len() > MAX_QUERY_TOKENS {
            return Err(error(
                start,
                SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::Tokens,
                    maximum: MAX_QUERY_TOKENS,
                },
            ));
        }
    }
    Ok(tokens)
}

/// Read a double- or single-quoted literal. `index` starts at the opening
/// delimiter and finishes immediately after its matching delimiter.
fn lex_quoted(
    input: &str,
    index: &mut usize,
    start: usize,
    delimiter: char,
) -> Result<String, SearchParseError> {
    *index += delimiter.len_utf8();
    let mut value = String::new();
    while *index < input.len() {
        let quoted = input[*index..]
            .chars()
            .next()
            .expect("valid character boundary");
        if quoted.is_control() {
            return Err(error(*index, SearchParseErrorKind::ControlCharacter));
        }
        if quoted == delimiter {
            *index += delimiter.len_utf8();
            validate_literal(&value, start)?;
            return Ok(value);
        }
        if quoted == '\\' {
            let escape_at = *index;
            *index += 1;
            let Some(escaped) = input[*index..].chars().next() else {
                return Err(error(escape_at, SearchParseErrorKind::InvalidEscape));
            };
            // Escaped quotes are accepted across delimiter styles. This keeps
            // pasted quoted text stable whether the user started with single
            // or double quotes, while still rejecting every other escape.
            if escaped != '\\' && !matches!(escaped, '\'' | '"') {
                return Err(error(escape_at, SearchParseErrorKind::InvalidEscape));
            }
            value.push(escaped);
            *index += escaped.len_utf8();
        } else {
            value.push(quoted);
            *index += quoted.len_utf8();
        }
    }
    Err(error(start, SearchParseErrorKind::UnterminatedQuote))
}

fn validate_literal(value: &str, position: usize) -> Result<(), SearchParseError> {
    if value.len() > MAX_QUERY_LITERAL_BYTES {
        return Err(error(
            position,
            SearchParseErrorKind::LimitExceeded {
                limit: SearchLimit::LiteralBytes,
                maximum: MAX_QUERY_LITERAL_BYTES,
            },
        ));
    }
    Ok(())
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
    operands: usize,
    depth: usize,
}

impl Parser {
    fn parse_or(&mut self) -> Result<SearchNode, SearchParseError> {
        let mut nodes = vec![self.parse_and()?];
        while self.is_keyword("OR") {
            let operator = self.next().expect("peeked OR");
            if !self.can_start_operand() {
                return Err(error(operator.end, SearchParseErrorKind::MissingOperand));
            }
            nodes.push(self.parse_and()?);
        }
        Ok(flatten_or(nodes))
    }

    fn parse_and(&mut self) -> Result<SearchNode, SearchParseError> {
        let mut nodes = vec![self.parse_unary()?];
        loop {
            if self.is_keyword("AND") {
                let operator = self.next().expect("peeked AND");
                if !self.can_start_operand() {
                    return Err(error(operator.end, SearchParseErrorKind::MissingOperand));
                }
                nodes.push(self.parse_unary()?);
            } else if self.can_start_operand() {
                nodes.push(self.parse_unary()?);
            } else {
                break;
            }
        }
        Ok(flatten_and(nodes))
    }

    fn parse_unary(&mut self) -> Result<SearchNode, SearchParseError> {
        if self.is_keyword("NOT") {
            let operator = self.next().expect("peeked NOT");
            if !self.can_start_operand() {
                return Err(error(operator.end, SearchParseErrorKind::MissingOperand));
            }
            return Ok(SearchNode::Not(Box::new(self.parse_unary()?)));
        }
        if self.matches(|kind| matches!(kind, TokenKind::Minus)) {
            let operator = self.next().expect("peeked minus");
            if !self.can_start_operand() {
                return Err(error(operator.end, SearchParseErrorKind::MissingOperand));
            }
            return Ok(SearchNode::Not(Box::new(self.parse_unary()?)));
        }
        if self.matches(|kind| matches!(kind, TokenKind::Plus)) {
            let operator = self.next().expect("peeked plus");
            if !self.can_start_operand() {
                return Err(error(operator.end, SearchParseErrorKind::MissingOperand));
            }
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<SearchNode, SearchParseError> {
        let Some(token) = self.next() else {
            return Err(error(0, SearchParseErrorKind::MissingOperand));
        };
        match token.kind {
            TokenKind::LParen => {
                self.depth += 1;
                if self.depth > MAX_QUERY_DEPTH {
                    return Err(error(
                        token.start,
                        SearchParseErrorKind::LimitExceeded {
                            limit: SearchLimit::Depth,
                            maximum: MAX_QUERY_DEPTH,
                        },
                    ));
                }
                if self.matches(|kind| matches!(kind, TokenKind::RParen)) {
                    return Err(error(token.end, SearchParseErrorKind::MissingOperand));
                }
                let node = self.parse_or()?;
                let Some(close) = self.next() else {
                    return Err(error(
                        token.start,
                        SearchParseErrorKind::UnexpectedToken { token: "(".into() },
                    ));
                };
                if !matches!(close.kind, TokenKind::RParen) {
                    return Err(error(
                        close.start,
                        SearchParseErrorKind::UnexpectedToken {
                            token: close.display(),
                        },
                    ));
                }
                self.depth -= 1;
                Ok(node)
            }
            TokenKind::RParen => Err(error(
                token.start,
                SearchParseErrorKind::UnexpectedToken { token: ")".into() },
            )),
            TokenKind::Plus | TokenKind::Minus => {
                Err(error(token.start, SearchParseErrorKind::MissingOperand))
            }
            TokenKind::Quoted(value) => self.term(
                SearchTerm::Text(text(value, true, token.start)?),
                token.start,
            ),
            TokenKind::Word(value) => self.parse_word(value, token.start),
        }
    }

    fn parse_word(
        &mut self,
        word: String,
        position: usize,
    ) -> Result<SearchNode, SearchParseError> {
        if let Some((operator, inline_value)) = word.split_once(':') {
            if operator.is_empty() {
                return Err(error(
                    position,
                    SearchParseErrorKind::UnknownOperator {
                        operator: operator.into(),
                    },
                ));
            }
            let (value, quoted, value_position) = if inline_value.is_empty() {
                let Some(token) = self.next() else {
                    return Err(error(
                        position + operator.len() + 1,
                        SearchParseErrorKind::MissingLiteral {
                            operator: operator.to_string(),
                        },
                    ));
                };
                match token.kind {
                    TokenKind::Word(value) => (value, false, token.start),
                    TokenKind::Quoted(value) => (value, true, token.start),
                    _ => {
                        return Err(error(
                            token.start,
                            SearchParseErrorKind::MissingLiteral {
                                operator: operator.to_string(),
                            },
                        ));
                    }
                }
            } else {
                (
                    inline_value.to_string(),
                    false,
                    position + operator.len() + 1,
                )
            };
            return self.parse_operator(operator, value, quoted, position, value_position);
        }
        if word == "AND" || word == "OR" || word == "NOT" {
            return Err(error(
                position,
                SearchParseErrorKind::UnexpectedToken { token: word },
            ));
        }
        self.term(SearchTerm::Text(text(word, false, position)?), position)
    }

    fn parse_operator(
        &mut self,
        operator: &str,
        value: String,
        quoted: bool,
        operator_position: usize,
        value_position: usize,
    ) -> Result<SearchNode, SearchParseError> {
        let normalized = operator.to_ascii_lowercase();
        if is_deferred_operator(&normalized) || is_deferred_operator_value(&normalized, &value) {
            return Err(error(
                operator_position,
                SearchParseErrorKind::UnsupportedOperator {
                    operator: if is_deferred_operator(&normalized) {
                        operator.to_string()
                    } else {
                        format!("{operator}:{value}")
                    },
                },
            ));
        }
        let node = match normalized.as_str() {
            "from" | "to" | "tonotcc" | "cc" | "bcc" | "with" | "subject" | "body" => {
                let field = match normalized.as_str() {
                    "from" => SearchField::From,
                    "to" => SearchField::To,
                    "tonotcc" => SearchField::ToNotCc,
                    "cc" => SearchField::Cc,
                    "bcc" => SearchField::Bcc,
                    "with" => SearchField::With,
                    "subject" => SearchField::Subject,
                    "body" => SearchField::Body,
                    _ => unreachable!("known search field"),
                };
                SearchNode::Term(SearchTerm::Field {
                    field,
                    value: text(value, quoted, value_position)?,
                })
            }
            "in" => SearchNode::Term(SearchTerm::Folder(folder(value, quoted, value_position)?)),
            "date" | "before" | "after" => {
                if quoted {
                    return Err(error(
                        value_position,
                        SearchParseErrorKind::InvalidLiteral {
                            value,
                            reason: "dates cannot be quoted",
                        },
                    ));
                }
                let comparison = match normalized.as_str() {
                    "date" => DateComparison::On,
                    "before" => DateComparison::Before,
                    "after" => DateComparison::After,
                    _ => unreachable!("known date operator"),
                };
                SearchNode::Term(SearchTerm::Date {
                    comparison,
                    value: date(value, value_position)?,
                })
            }
            "has" => SearchNode::Term(SearchTerm::Attachment(attachment(
                value,
                quoted,
                value_position,
            )?)),
            "filename" => {
                SearchNode::Term(SearchTerm::Filename(text(value, quoted, value_position)?))
            }
            "filetype" => SearchNode::Term(SearchTerm::FileType(file_type(
                value,
                quoted,
                value_position,
            )?)),
            "is" | "state" => SearchNode::Term(SearchTerm::State(message_state(
                value,
                quoted,
                value_position,
            )?)),
            _ => {
                return Err(error(
                    operator_position,
                    SearchParseErrorKind::UnknownOperator {
                        operator: operator.to_string(),
                    },
                ));
            }
        };
        self.term_node(node, operator_position)
    }

    fn term(&mut self, term: SearchTerm, position: usize) -> Result<SearchNode, SearchParseError> {
        self.term_node(SearchNode::Term(term), position)
    }

    fn term_node(
        &mut self,
        node: SearchNode,
        position: usize,
    ) -> Result<SearchNode, SearchParseError> {
        self.operands += 1;
        if self.operands > MAX_QUERY_OPERANDS {
            return Err(error(
                position,
                SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::Operands,
                    maximum: MAX_QUERY_OPERANDS,
                },
            ));
        }
        Ok(node)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.cursor)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.cursor).cloned();
        if token.is_some() {
            self.cursor += 1;
        }
        token
    }

    fn matches(&self, predicate: impl FnOnce(&TokenKind) -> bool) -> bool {
        self.peek().is_some_and(|token| predicate(&token.kind))
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek().map(|token| &token.kind), Some(TokenKind::Word(word)) if word == keyword)
    }

    fn can_start_operand(&self) -> bool {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Quoted(_) | TokenKind::LParen | TokenKind::Plus | TokenKind::Minus) => {
                true
            }
            Some(TokenKind::Word(word)) => word != "AND" && word != "OR",
            _ => false,
        }
    }
}

fn flatten_and(nodes: Vec<SearchNode>) -> SearchNode {
    if nodes.len() == 1 {
        return nodes.into_iter().next().expect("one item");
    }
    let mut flattened = Vec::new();
    for node in nodes {
        match node {
            SearchNode::And(inner) => flattened.extend(inner),
            node => flattened.push(node),
        }
    }
    SearchNode::And(flattened)
}

fn flatten_or(nodes: Vec<SearchNode>) -> SearchNode {
    if nodes.len() == 1 {
        return nodes.into_iter().next().expect("one item");
    }
    let mut flattened = Vec::new();
    for node in nodes {
        match node {
            SearchNode::Or(inner) => flattened.extend(inner),
            node => flattened.push(node),
        }
    }
    SearchNode::Or(flattened)
}

fn text(value: String, quoted: bool, position: usize) -> Result<SearchText, SearchParseError> {
    if value.is_empty() {
        return Err(error(
            position,
            SearchParseErrorKind::InvalidLiteral {
                value,
                reason: "text cannot be empty",
            },
        ));
    }
    if quoted {
        // Quotes select adjacent-word matching. Asterisks and regex-looking
        // punctuation remain literal input because this language has no regex
        // mode and prefix matching is only the unquoted trailing `*` form.
        return Ok(SearchText {
            value,
            quoted: true,
            prefix: false,
        });
    }
    let prefix = value.ends_with('*');
    let trimmed = if prefix {
        &value[..value.len() - 1]
    } else {
        &value
    };
    if trimmed.is_empty() || trimmed.contains('*') {
        return Err(error(
            position,
            SearchParseErrorKind::InvalidLiteral {
                value,
                reason: "only a trailing * is supported for prefix matching",
            },
        ));
    }
    Ok(SearchText {
        value: trimmed.to_string(),
        quoted: false,
        prefix,
    })
}

fn folder(value: String, quoted: bool, position: usize) -> Result<FolderScope, SearchParseError> {
    if value.is_empty() {
        return Err(invalid(value, position, "folder cannot be empty"));
    }
    if !quoted && value == "*" {
        return Ok(FolderScope::All);
    }
    if value.ends_with("/*") {
        let base = &value[..value.len() - 2];
        if base.is_empty() || base.contains('*') {
            return Err(invalid(
                value,
                position,
                "only a trailing /* is supported for nested folders",
            ));
        }
        return Ok(FolderScope::Descendants(base.to_string()));
    }
    // Brackets and other punctuation are ordinary mailbox-path characters for
    // providers such as Gmail. Only `*` has mailbox wildcard meaning.
    if value.contains('*') {
        return Err(invalid(
            value,
            position,
            "folder wildcards are limited to in:* and a trailing /*",
        ));
    }
    Ok(FolderScope::Exact(value))
}

fn date(value: String, position: usize) -> Result<SearchDate, SearchParseError> {
    if let Ok(date) = NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
        return Ok(SearchDate::Absolute(date));
    }
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, suffix) = value.split_at(split);
    let amount = amount.parse::<u32>().ok().filter(|amount| *amount > 0);
    let unit = match suffix {
        "d" => Some(RelativeDateUnit::Days),
        "w" => Some(RelativeDateUnit::Weeks),
        "m" => Some(RelativeDateUnit::Months),
        "y" => Some(RelativeDateUnit::Years),
        _ => None,
    };
    match (amount, unit) {
        (Some(amount), Some(unit)) => Ok(SearchDate::Relative { amount, unit }),
        _ => Err(error(position, SearchParseErrorKind::InvalidDate { value })),
    }
}

fn attachment(
    value: String,
    quoted: bool,
    position: usize,
) -> Result<AttachmentPredicate, SearchParseError> {
    if quoted {
        return Err(invalid(
            value,
            position,
            "attachment state cannot be quoted",
        ));
    }
    match value.to_ascii_lowercase().as_str() {
        "attachment" | "attachments" | "att" | "file" | "files" => {
            Ok(AttachmentPredicate::HasAttachment)
        }
        "noattachment" | "noattachments" | "noatt" | "nofile" | "nofiles" => {
            Ok(AttachmentPredicate::HasNoAttachment)
        }
        _ => Err(invalid(
            value,
            position,
            "expected attachment or noattachment",
        )),
    }
}

fn file_type(value: String, quoted: bool, position: usize) -> Result<FileType, SearchParseError> {
    if quoted {
        return Err(invalid(value, position, "file type cannot be quoted"));
    }
    match value.to_ascii_lowercase().as_str() {
        "pdf" => Ok(FileType::Pdf),
        "document" | "documents" | "doc" => Ok(FileType::Document),
        "spreadsheet" | "spreadsheets" => Ok(FileType::Spreadsheet),
        "presentation" | "presentations" => Ok(FileType::Presentation),
        "image" | "images" => Ok(FileType::Image),
        "audio" => Ok(FileType::Audio),
        "video" => Ok(FileType::Video),
        "archive" | "archives" => Ok(FileType::Archive),
        "calendar" => Ok(FileType::Calendar),
        _ => Err(invalid(value, position, "unknown file type")),
    }
}

fn message_state(
    value: String,
    quoted: bool,
    position: usize,
) -> Result<MessageState, SearchParseError> {
    if quoted {
        return Err(invalid(value, position, "message state cannot be quoted"));
    }
    match value.to_ascii_lowercase().as_str() {
        "read" | "seen" => Ok(MessageState::Read),
        "unread" | "unseen" => Ok(MessageState::Unread),
        "pinned" | "flagged" => Ok(MessageState::Flagged),
        "unpinned" | "unflagged" => Ok(MessageState::Unflagged),
        "replied" | "answered" => Ok(MessageState::Replied),
        "unreplied" | "unanswered" => Ok(MessageState::Unreplied),
        "draft" => Ok(MessageState::Draft),
        "undraft" => Ok(MessageState::Undraft),
        _ => Err(invalid(value, position, "unknown message state")),
    }
}

fn is_deferred_operator(operator: &str) -> bool {
    matches!(
        operator,
        "memo"
            | "attached"
            | "header"
            | "msgid"
            | "messageid"
            | "list"
            | "listid"
            | "priority"
            | "size"
            | "bigger"
            | "minsize"
            | "maxsize"
            | "larger"
            | "smaller"
            | "largerthan"
            | "smallerthan"
            | "bytes"
            | "mute"
            | "muted"
            | "flag"
            | "flags"
            | "keyword"
            | "label"
            | "userlabel"
            | "tag"
            | "fromin"
            | "toin"
            | "ccin"
            | "bccin"
            | "within"
            | "group"
            | "contact"
            | "contacts"
            | "deliveredto"
            | "received"
            | "sent"
            | "inreplyto"
            | "references"
            | "thread"
    )
}

/// Some documented-but-deferred constructs are values of otherwise supported
/// operators. Classify them before field parsing so callers receive a stable
/// Unsupported error rather than an accidental "unknown state" or attachment
/// literal error.
fn is_deferred_operator_value(operator: &str, value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    matches!(
        (operator, value.as_str()),
        (
            "has",
            "memo" | "nomemo" | "userlabels" | "userlabel" | "labels"
        ) | ("is" | "state", "muted" | "mute")
    )
}

fn invalid(value: String, position: usize, reason: &'static str) -> SearchParseError {
    error(
        position,
        SearchParseErrorKind::InvalidLiteral { value, reason },
    )
}

fn error(position: usize, kind: SearchParseErrorKind) -> SearchParseError {
    SearchParseError { position, kind }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(value: &str) -> SearchNode {
        SearchNode::Term(SearchTerm::Text(SearchText {
            value: value.into(),
            quoted: false,
            prefix: false,
        }))
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(
            parse_search_query("  ").unwrap(),
            SearchExpression::match_all()
        );
    }

    #[test]
    fn operator_precedence_is_not_ambiguous() {
        assert_eq!(
            parse_search_query("alpha OR beta AND NOT gamma")
                .unwrap()
                .root,
            SearchNode::Or(vec![
                term("alpha"),
                SearchNode::And(vec![term("beta"), SearchNode::Not(Box::new(term("gamma")))]),
            ])
        );
        assert_eq!(
            parse_search_query("(alpha OR beta) gamma").unwrap().root,
            SearchNode::And(vec![
                SearchNode::Or(vec![term("alpha"), term("beta")]),
                term("gamma")
            ])
        );
    }

    #[test]
    fn supports_quotes_prefixes_unicode_and_addresses() {
        let parsed =
            parse_search_query("subject:\"Quarterly report\" from:Åsa+work@example.test invoic*")
                .unwrap();
        assert_eq!(
            parsed.root,
            SearchNode::And(vec![
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::Subject,
                    value: SearchText {
                        value: "Quarterly report".into(),
                        quoted: true,
                        prefix: false
                    },
                }),
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::From,
                    value: SearchText {
                        value: "Åsa+work@example.test".into(),
                        quoted: false,
                        prefix: false
                    },
                }),
                SearchNode::Term(SearchTerm::Text(SearchText {
                    value: "invoic".into(),
                    quoted: false,
                    prefix: true
                })),
            ])
        );
    }

    #[test]
    fn supports_single_quoted_phrases_and_field_values() {
        assert_eq!(
            parse_search_query(
                "subject:'Quarterly report' from:'Aoife O\\\'Brien <aoife@example.test>'"
            )
            .unwrap()
            .root,
            SearchNode::And(vec![
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::Subject,
                    value: SearchText {
                        value: "Quarterly report".into(),
                        quoted: true,
                        prefix: false,
                    },
                }),
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::From,
                    value: SearchText {
                        value: "Aoife O'Brien <aoife@example.test>".into(),
                        quoted: true,
                        prefix: false,
                    },
                }),
            ])
        );
        assert_eq!(
            parse_search_query("'back\\\\slash'").unwrap().root,
            SearchNode::Term(SearchTerm::Text(SearchText {
                value: "back\\slash".into(),
                quoted: true,
                prefix: false,
            }))
        );
    }

    #[test]
    fn accepts_cross_delimiter_escaped_quotes_and_backslashes() {
        for (input, expected) in [
            (r#"'a \"double\" quote'"#, "a \"double\" quote"),
            (r#""a \'single\' quote""#, "a 'single' quote"),
            (r#"'two \\ slashes'"#, "two \\ slashes"),
            (r#""two \\ slashes""#, "two \\ slashes"),
        ] {
            assert_eq!(
                parse_search_query(input).unwrap().root,
                SearchNode::Term(SearchTerm::Text(SearchText {
                    value: expected.into(),
                    quoted: true,
                    prefix: false,
                })),
                "{input}"
            );
        }
    }

    #[test]
    fn regex_looking_punctuation_is_plain_search_text() {
        for input in [
            "subject:\"[EXTERNAL] Quarterly report\"",
            "price$ update",
            "^",
            ".*",
            "[notice]",
            "subject:/invoice/",
            "in:[Gmail]/Drafts",
        ] {
            assert!(parse_search_query(input).is_ok(), "{input}");
        }
        assert!(matches!(
            parse_search_query("in:Pro*jects"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::InvalidLiteral { .. },
                ..
            })
        ));
    }

    #[test]
    fn recognizes_all_fields_and_special_predicates() {
        let parsed = parse_search_query(
            "to:one tonotcc:two cc:three bcc:four with:five body:six in:Projects/* date:2026-09-06 before:2w after:1m has:attachments filename:invoice* filetype:pdf is:answered",
        )
        .unwrap();
        let SearchNode::And(terms) = parsed.root else {
            panic!("implicit AND")
        };
        assert_eq!(terms.len(), 14);
        assert!(
            matches!(terms[6], SearchNode::Term(SearchTerm::Folder(FolderScope::Descendants(ref path))) if path == "Projects")
        );
        assert!(matches!(
            terms[7],
            SearchNode::Term(SearchTerm::Date {
                comparison: DateComparison::On,
                value: SearchDate::Absolute(_)
            })
        ));
        assert!(matches!(
            terms[8],
            SearchNode::Term(SearchTerm::Date {
                comparison: DateComparison::Before,
                value: SearchDate::Relative {
                    amount: 2,
                    unit: RelativeDateUnit::Weeks
                }
            })
        ));
        assert!(matches!(
            terms[11],
            SearchNode::Term(SearchTerm::Filename(SearchText { prefix: true, .. }))
        ));
        assert!(matches!(
            terms[13],
            SearchNode::Term(SearchTerm::State(MessageState::Replied))
        ));
    }

    #[test]
    fn quoted_provider_mailbox_paths_keep_literal_brackets() {
        assert_eq!(
            parse_search_query("in:\"[Gmail]/Drafts\"").unwrap().root,
            SearchNode::Term(SearchTerm::Folder(FolderScope::Exact(
                "[Gmail]/Drafts".into()
            )))
        );
    }

    #[test]
    fn aliases_are_canonicalized() {
        let parsed = parse_search_query(
            "has:nofiles has:att has:noatt is:seen state:unpinned state:unanswered",
        )
        .unwrap();
        let SearchNode::And(terms) = parsed.root else {
            panic!("implicit AND")
        };
        assert_eq!(
            terms[0],
            SearchNode::Term(SearchTerm::Attachment(AttachmentPredicate::HasNoAttachment))
        );
        assert_eq!(
            terms[1],
            SearchNode::Term(SearchTerm::Attachment(AttachmentPredicate::HasAttachment))
        );
        assert_eq!(
            terms[2],
            SearchNode::Term(SearchTerm::Attachment(AttachmentPredicate::HasNoAttachment))
        );
        assert_eq!(
            terms[3],
            SearchNode::Term(SearchTerm::State(MessageState::Read))
        );
        assert_eq!(
            terms[4],
            SearchNode::Term(SearchTerm::State(MessageState::Unflagged))
        );
        assert_eq!(
            terms[5],
            SearchNode::Term(SearchTerm::State(MessageState::Unreplied))
        );
    }

    #[test]
    fn rejects_control_and_injection_characters_before_interpretation() {
        for input in ["from:alice\r\nOR subject:invoice", "body:hello\0world"] {
            assert!(matches!(
                parse_search_query(input),
                Err(SearchParseError {
                    kind: SearchParseErrorKind::ControlCharacter,
                    ..
                })
            ));
        }
    }

    #[test]
    fn rejects_invalid_syntax_and_unsupported_operators() {
        assert!(matches!(
            parse_search_query("alpha OR"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::MissingOperand,
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("from:"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::MissingLiteral { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("unknown:value"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::UnknownOperator { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("memo:value"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::UnsupportedOperator { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("has:userlabels"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::UnsupportedOperator { .. },
                ..
            })
        ));
        for input in [
            "fromin:Friends",
            "toin:Team",
            "size:10M",
            "larger:5M",
            "bigger:5M",
            "minsize:5M",
            "maxsize:5M",
            "flag:blue",
            "has:memo",
            "has:nomemo",
            "is:muted",
        ] {
            assert!(
                matches!(
                    parse_search_query(input),
                    Err(SearchParseError {
                        kind: SearchParseErrorKind::UnsupportedOperator { .. },
                        ..
                    })
                ),
                "{input} must be reported as deferred, not parsed as an ordinary error"
            );
        }
        assert!(matches!(
            parse_search_query("(alpha"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::UnexpectedToken { .. },
                ..
            })
        ));
    }

    #[test]
    fn rejects_bad_dates_and_escapes() {
        assert!(matches!(
            parse_search_query("after:2026-02-30"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::InvalidDate { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("before:0d"),
            Err(SearchParseError {
                kind: SearchParseErrorKind::InvalidDate { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_search_query("\"bad\\q\""),
            Err(SearchParseError {
                kind: SearchParseErrorKind::InvalidEscape,
                ..
            })
        ));
        assert_eq!(
            parse_search_query("\"a \\\"quote\\\" and \\\\ slash\"")
                .unwrap()
                .root,
            SearchNode::Term(SearchTerm::Text(SearchText {
                value: "a \"quote\" and \\ slash".into(),
                quoted: true,
                prefix: false
            }))
        );
        let single_quote_escape = parse_search_query("\u{00e5} 'bad\\q'").unwrap_err();
        assert_eq!(single_quote_escape.position, 7);
        assert_eq!(
            single_quote_escape.kind,
            SearchParseErrorKind::InvalidEscape
        );
        let unterminated = parse_search_query("\u{00e5} 'unterminated").unwrap_err();
        assert_eq!(unterminated.position, 3);
        assert_eq!(unterminated.kind, SearchParseErrorKind::UnterminatedQuote);
    }

    #[test]
    fn enforces_complexity_limits() {
        let too_deep = format!(
            "{}x{}",
            "(".repeat(MAX_QUERY_DEPTH + 1),
            ")".repeat(MAX_QUERY_DEPTH + 1)
        );
        assert!(matches!(
            parse_search_query(&too_deep),
            Err(SearchParseError {
                kind: SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::Depth,
                    ..
                },
                ..
            })
        ));
        let too_long = "x".repeat(MAX_QUERY_BYTES + 1);
        let error = parse_search_query(&too_long).unwrap_err();
        assert_eq!(error.position, MAX_QUERY_BYTES);
        assert!(matches!(
            error.kind,
            SearchParseErrorKind::LimitExceeded {
                limit: SearchLimit::QueryBytes,
                ..
            }
        ));
        let operands = std::iter::repeat_n("x", MAX_QUERY_OPERANDS + 1)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(matches!(
            parse_search_query(&operands),
            Err(SearchParseError {
                kind: SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::Operands,
                    ..
                },
                ..
            })
        ));
        let too_many_tokens = "+ ".repeat(MAX_QUERY_TOKENS + 1);
        assert!(matches!(
            parse_search_query(&too_many_tokens),
            Err(SearchParseError {
                kind: SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::Tokens,
                    ..
                },
                ..
            })
        ));
        let too_long_literal = "x".repeat(MAX_QUERY_LITERAL_BYTES + 1);
        assert!(matches!(
            parse_search_query(&too_long_literal),
            Err(SearchParseError {
                kind: SearchParseErrorKind::LimitExceeded {
                    limit: SearchLimit::LiteralBytes,
                    ..
                },
                ..
            })
        ));
    }
}
