//! Conservative SQLite candidate compiler for the shared search AST.
//!
//! The compiler deliberately produces a *superset* of the canonical
//! evaluator's matches.  Callers must evaluate each returned message with
//! [`crate::search_eval::evaluate_search`] before publishing it.  This lets
//! SQLite/FTS narrow the common, indexed cases without assigning a different
//! meaning to a query on one client or provider.
//!
//! ## Integrating with sqlx
//!
//! Build a `SELECT ... FROM messages m WHERE {predicate}` statement using the
//! returned predicate, then bind every item in `binds` in order.  The fragment
//! contains only constants owned by this module; values from the search query
//! are represented exclusively by a `?` placeholder and `SqlSearchBind`.

use crate::search::{
    AttachmentPredicate, DateComparison, FileType, FolderScope, MessageState, RelativeDateUnit,
    SearchDate, SearchExpression, SearchField, SearchNode, SearchTerm, SearchText,
};
use chrono::{Datelike, Duration, NaiveDate};
use std::fmt;

/// A value to bind to a `?` placeholder in [`SqlSearchCandidate::predicate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSearchBind {
    Text(String),
    Integer(i64),
}

/// A SQLite `WHERE` fragment for the message-table alias `m`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlSearchCandidate {
    /// A bound `WHERE` expression. It never includes raw search text.
    pub predicate: String,
    /// Ordered values for the placeholders in `predicate`.
    pub binds: Vec<SqlSearchBind>,
    /// Whether the fragment deliberately omitted part of the AST. This is
    /// expected for Unicode text and any predicate whose local index is not a
    /// safe complete representation.
    pub requires_post_filter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlSearchCompileError {
    pub kind: SqlSearchCompileErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSearchCompileErrorKind {
    ControlCharacter,
}

impl fmt::Display for SqlSearchCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            SqlSearchCompileErrorKind::ControlCharacter => {
                write!(
                    formatter,
                    "SQLite search literals cannot contain control characters"
                )
            }
        }
    }
}

impl std::error::Error for SqlSearchCompileError {}

/// Compiles the parsed AST into a bound SQLite candidate predicate.
///
/// `today` makes relative dates replayable and is intentionally supplied by
/// the caller just like the canonical evaluator. The resulting candidate must
/// be post-filtered by that evaluator before display.
pub fn compile_sql_candidate(
    expression: &SearchExpression,
    today: NaiveDate,
) -> Result<SqlSearchCandidate, SqlSearchCompileError> {
    validate_node(&expression.root)?;
    let compiled = compile_node(&expression.root, today)?;
    Ok(SqlSearchCandidate {
        predicate: compiled.predicate.unwrap_or_else(|| "1 = 1".into()),
        binds: compiled.binds,
        requires_post_filter: compiled.omitted,
    })
}

struct CompiledNode {
    predicate: Option<String>,
    binds: Vec<SqlSearchBind>,
    omitted: bool,
}

impl CompiledNode {
    fn all() -> Self {
        Self {
            predicate: Some("1 = 1".into()),
            binds: Vec::new(),
            omitted: false,
        }
    }
}

fn compile_node(
    node: &SearchNode,
    today: NaiveDate,
) -> Result<CompiledNode, SqlSearchCompileError> {
    match node {
        SearchNode::MatchAll => Ok(CompiledNode::all()),
        SearchNode::Term(term) => compile_term(term, today),
        SearchNode::And(nodes) => combine_and(nodes, today),
        SearchNode::Or(nodes) => combine_or(nodes, today),
        // Negating a partial candidate could hide a canonical match. Only use
        // it when the child was represented exactly by SQL predicates.
        SearchNode::Not(node) => {
            let child = compile_node(node, today)?;
            if child.omitted || child.predicate.is_none() {
                Ok(CompiledNode {
                    predicate: None,
                    binds: Vec::new(),
                    omitted: true,
                })
            } else {
                Ok(CompiledNode {
                    predicate: child
                        .predicate
                        .map(wrap)
                        .map(|predicate| format!("NOT {predicate}")),
                    binds: child.binds,
                    omitted: false,
                })
            }
        }
    }
}

fn combine_and(
    nodes: &[SearchNode],
    today: NaiveDate,
) -> Result<CompiledNode, SqlSearchCompileError> {
    let mut predicates = Vec::new();
    let mut binds = Vec::new();
    let mut omitted = false;
    for node in nodes {
        let child = compile_node(node, today)?;
        omitted |= child.omitted || child.predicate.is_none();
        if let Some(predicate) = child.predicate {
            predicates.push(wrap(predicate));
            binds.extend(child.binds);
        }
    }
    Ok(CompiledNode {
        predicate: (!predicates.is_empty()).then(|| predicates.join(" AND ")),
        binds,
        omitted,
    })
}

fn combine_or(
    nodes: &[SearchNode],
    today: NaiveDate,
) -> Result<CompiledNode, SqlSearchCompileError> {
    let mut predicates = Vec::new();
    let mut binds = Vec::new();
    let mut omitted = false;
    for node in nodes {
        let child = compile_node(node, today)?;
        // An omitted OR branch means that branch's candidate set is all rows,
        // so retaining any other branch would be a false-negative filter.
        // This also applies to an incomplete FTS index: it may help a positive
        // AND, but cannot safely decide one branch of an OR.
        let Some(predicate) = child.predicate else {
            return Ok(CompiledNode {
                predicate: None,
                binds: Vec::new(),
                omitted: true,
            });
        };
        if child.omitted {
            return Ok(CompiledNode {
                predicate: None,
                binds: Vec::new(),
                omitted: true,
            });
        }
        omitted |= child.omitted;
        predicates.push(wrap(predicate));
        binds.extend(child.binds);
    }
    Ok(CompiledNode {
        predicate: (!predicates.is_empty()).then(|| predicates.join(" OR ")),
        binds,
        omitted,
    })
}

fn compile_term(
    term: &SearchTerm,
    today: NaiveDate,
) -> Result<CompiledNode, SqlSearchCompileError> {
    let predicate = match term {
        SearchTerm::Text(text) => fts_any_header_or_cached_body(text),
        SearchTerm::Field { field, value } => fts_field(*field, value),
        SearchTerm::Folder(scope) => folder_predicate(scope),
        SearchTerm::Date { comparison, value } => date_predicate(*comparison, value, today),
        SearchTerm::Attachment(predicate) => attachment_predicate(*predicate),
        SearchTerm::Filename(text) => filename_predicate(text),
        SearchTerm::FileType(file_type) => file_type_predicate(*file_type),
        SearchTerm::State(state) => Some(CompiledNode {
            predicate: Some(
                match state {
                    MessageState::Read => "m.is_read = 1",
                    MessageState::Unread => "m.is_read = 0",
                    MessageState::Flagged => "m.is_flagged = 1",
                    MessageState::Unflagged => "m.is_flagged = 0",
                    MessageState::Replied => "m.is_answered = 1",
                    MessageState::Unreplied => "m.is_answered = 0",
                    MessageState::Draft => "m.is_draft = 1",
                    MessageState::Undraft => "m.is_draft = 0",
                }
                .into(),
            ),
            binds: Vec::new(),
            omitted: false,
        }),
    };
    Ok(predicate.unwrap_or(CompiledNode {
        predicate: None,
        binds: Vec::new(),
        omitted: true,
    }))
}

fn fts_any_header_or_cached_body(text: &SearchText) -> Option<CompiledNode> {
    let fts = fts_query(text)?;
    let (body_predicate, mut body_binds) = body_candidate_predicate(text)?;
    let header_non_ascii = non_ascii_any(&[
        "m.from_name",
        "m.from_address",
        "m.to_addresses",
        "m.cc_addresses",
        "m.bcc_addresses",
        "m.subject",
        "m.snippet",
    ]);
    let mut binds = vec![SqlSearchBind::Text(fts)];
    binds.append(&mut body_binds);
    Some(CompiledNode {
        predicate: Some(format!(
            "(m.rowid IN (SELECT rowid FROM messages_fts_v2 WHERE messages_fts_v2 MATCH ?) \
             OR {body_predicate} \
             OR {header_non_ascii})"
        )),
        binds,
        // The durable catalogue intentionally indexes snippets, while body
        // text is only complete when cached. Keep evaluation authoritative.
        omitted: true,
    })
}

fn fts_field(field: SearchField, text: &SearchText) -> Option<CompiledNode> {
    let fts = fts_query(text)?;
    let columns: &[&str] = match field {
        SearchField::From => &["from_name", "from_address"],
        // Fastmail's `to:` is recipient-wide. `tonotcc:` remains the explicit
        // To-header-only form, even when the same address also appears in Cc.
        SearchField::To => &["to_addresses", "cc_addresses", "bcc_addresses"],
        SearchField::Cc => &["cc_addresses"],
        SearchField::Bcc => &["bcc_addresses"],
        SearchField::With => &[
            "from_name",
            "from_address",
            "to_addresses",
            "cc_addresses",
            "bcc_addresses",
        ],
        SearchField::Subject => &["subject"],
        SearchField::Body => return cached_body_fts(text),
        SearchField::ToNotCc => return to_not_cc_fts(text),
    };

    let mut predicates = Vec::new();
    let mut binds = Vec::new();
    for column in columns {
        predicates.push(
            "m.rowid IN (SELECT rowid FROM messages_fts_v2 WHERE messages_fts_v2 MATCH ?)".into(),
        );
        binds.push(SqlSearchBind::Text(format!("{column} : {fts}")));
    }
    // FTS5's unicode61 tokenizer is a valuable ASCII fast path, but does not
    // implement all of the canonical evaluator's folding (for example,
    // `strasse` must match `Straße`). Send every non-ASCII value through Rust
    // as well so this remains a candidate superset.
    predicates.push(join_or(
        columns
            .iter()
            .map(|column| non_ascii_value(&format!("m.{column}")))
            .collect(),
    ));
    Some(CompiledNode {
        predicate: Some(join_or(predicates)),
        binds,
        // FTS only sees the v2 local catalogue and does not implement all
        // Unicode normalization rules of the canonical evaluator.
        omitted: true,
    })
}

fn to_not_cc_fts(text: &SearchText) -> Option<CompiledNode> {
    let fts = fts_query(text)?;
    let non_ascii_to = non_ascii_value("m.to_addresses");
    Some(CompiledNode {
        predicate: Some(
            format!(
                "(m.rowid IN (SELECT rowid FROM messages_fts_v2 WHERE messages_fts_v2 MATCH ?) OR {non_ascii_to})"
            ),
        ),
        binds: vec![SqlSearchBind::Text(format!("to_addresses : {fts}"))],
        // `tonotcc:` means the literal To header only. It does not negate Cc,
        // so a recipient present in both To and Cc is still a match.
        omitted: true,
    })
}

fn cached_body_fts(text: &SearchText) -> Option<CompiledNode> {
    let (predicate, binds) = body_candidate_predicate(text)?;
    Some(CompiledNode {
        predicate: Some(predicate),
        binds,
        omitted: true,
    })
}

/// Candidate terms for every table that can become `SearchableMessage.body`
/// in `Store::search_matching_messages`. Cached/search body tables are FTS
/// indexed; starred and legacy message bodies are not, so use deliberately
/// broad ASCII `LIKE` checks plus a non-ASCII fallback for those rows.
fn body_candidate_predicate(text: &SearchText) -> Option<(String, Vec<SqlSearchBind>)> {
    let fts = fts_query(text)?;
    let (legacy_body, mut legacy_binds) = ascii_like_all("m.body_text", text)?;
    let (starred_body, mut starred_binds) = ascii_like_all("b.body_text", text)?;
    let starred =
        format!("m.id IN (SELECT b.message_id FROM starred_message_bodies b WHERE {starred_body})");
    let non_ascii = join_or(vec![
        non_ascii_value("m.body_text"),
        format!(
            "m.id IN (SELECT c.message_id FROM message_content_cache c WHERE {})",
            non_ascii_value("c.body_text")
        ),
        format!(
            "m.id IN (SELECT s.message_id FROM message_search_body_text s WHERE {})",
            non_ascii_value("s.body_text")
        ),
        format!(
            "m.id IN (SELECT b.message_id FROM starred_message_bodies b WHERE {})",
            non_ascii_value("b.body_text")
        ),
    ]);
    let mut binds = vec![SqlSearchBind::Text(fts.clone()), SqlSearchBind::Text(fts)];
    binds.append(&mut legacy_binds);
    binds.append(&mut starred_binds);
    Some((
        format!(
            "(m.id IN (SELECT message_id FROM message_cached_bodies_fts WHERE message_cached_bodies_fts MATCH ?) \
             OR m.id IN (SELECT message_id FROM message_search_bodies_fts WHERE message_search_bodies_fts MATCH ?) \
             OR ({legacy_body}) \
             OR {starred} \
             OR {non_ascii})"
        ),
        binds,
    ))
}

fn folder_predicate(scope: &FolderScope) -> Option<CompiledNode> {
    match scope {
        FolderScope::All => Some(CompiledNode::all()),
        // SQLite's NOCASE collation is only ASCII. Let the evaluator handle
        // folder names outside that subset rather than narrowing incorrectly.
        FolderScope::Exact(folder) if folder.is_ascii() => {
            let (membership, mut binds) = membership_folder_predicate(folder, false);
            let mut predicates = vec![membership, "m.mailbox = ? COLLATE NOCASE".into()];
            binds.push(SqlSearchBind::Text(folder.clone()));
            if !folder.contains("::") {
                // Special-use mailboxes retain an opaque `Family::remote`
                // storage identity. A user may search the visible remote
                // alias, such as `[Gmail]/Drafts`, but never the opaque `::`
                // namespace itself. The leading wildcard is ours; all user
                // wildcard characters stay escaped inside the bound value.
                predicates.push("m.mailbox LIKE ? ESCAPE '\\' COLLATE NOCASE".into());
                binds.push(SqlSearchBind::Text(format!("%::{}", escape_like(folder))));
            }
            if special_mailbox_family(folder) {
                // A provider can expose several remote folders carrying the
                // same special-use role. They are stored as, for example,
                // `Sent::Sent Messages`, but are one user-visible Sent scope.
                predicates.push("m.mailbox LIKE ? ESCAPE '\\' COLLATE NOCASE".into());
                binds.push(SqlSearchBind::Text(format!("{folder}::%")));
            }
            // Provider mailbox storage locators retain a raw IMAP identity
            // separate from their display path. Their decoded path belongs to
            // the canonical evaluator, so include every opaque generic row as
            // a positive candidate rather than risking a false negative.
            predicates.push("m.mailbox LIKE 'Mailbox::@dakia-mailbox-v1:%' ESCAPE '\\'".into());
            predicates.push("m.mailbox LIKE '%::@dakia-special-v1:%' ESCAPE '\\'".into());
            predicates.push(non_ascii_value("m.mailbox"));
            Some(CompiledNode {
                predicate: Some(join_or(predicates)),
                binds,
                // ASCII SQLite comparisons are useful positive candidates,
                // but the final non-ASCII branch deliberately admits rows
                // for Rust's Unicode/diacritic comparison. Never negate that
                // broad candidate: `NOT in:Archive` must retain a mailbox
                // such as `Århus` rather than excluding every non-ASCII name.
                omitted: true,
            })
        }
        FolderScope::Descendants(folder) if folder.is_ascii() => {
            let (membership, mut binds) = membership_folder_predicate(folder, true);
            let mut predicates = vec![
                membership,
                "m.mailbox = ? COLLATE NOCASE".into(),
                "m.mailbox LIKE ? ESCAPE '\\' COLLATE NOCASE".into(),
            ];
            binds.extend([
                SqlSearchBind::Text(folder.clone()),
                SqlSearchBind::Text(format!("{}/%", escape_like(folder))),
            ]);
            if !folder.contains("::") {
                // The `/*` operator follows the visible provider hierarchy.
                // `Drafts::[Gmail]/Drafts` therefore matches
                // `in:"[Gmail]/*"`, while `in:Drafts/*` does not accidentally
                // expose every opaque Drafts alias as a child.
                predicates.push("m.mailbox LIKE ? ESCAPE '\\' COLLATE NOCASE".into());
                predicates.push("m.mailbox LIKE ? ESCAPE '\\' COLLATE NOCASE".into());
                binds.push(SqlSearchBind::Text(format!("%::{}", escape_like(folder))));
                binds.push(SqlSearchBind::Text(format!("%::{}/%", escape_like(folder))));
            }
            predicates.push("m.mailbox LIKE 'Mailbox::@dakia-mailbox-v1:%' ESCAPE '\\'".into());
            predicates.push("m.mailbox LIKE '%::@dakia-special-v1:%' ESCAPE '\\'".into());
            predicates.push(non_ascii_value("m.mailbox"));
            Some(CompiledNode {
                predicate: Some(join_or(predicates)),
                binds,
                // See Exact above. The Unicode fallback is a superset and is
                // only safe as a positive candidate predicate.
                omitted: true,
            })
        }
        _ => None,
    }
}

/// Matches a user-visible mailbox membership for this physical row or a
/// same-account physical alias with the same canonical RFC Message-ID. The
/// latter is deliberately a SQLite candidate only: the Rust evaluator loads
/// those paths and remains authoritative for every folder expression.
///
/// `message_id` values are persisted as RFC header text, so lowercased,
/// trimmed equality is the SQL representation of the canonical one-ID form.
/// Rows without an RFC Message-ID still retain their own membership through
/// the direct `logical.id = m.id` branch and their legacy mailbox fallback.
fn membership_folder_predicate(folder: &str, descendants: bool) -> (String, Vec<SqlSearchBind>) {
    let mut paths = vec!["sm.local_path = ? COLLATE NOCASE".into()];
    let mut binds = vec![SqlSearchBind::Text(folder.to_owned())];
    if descendants {
        paths.push("sm.local_path LIKE ? ESCAPE '\\' COLLATE NOCASE".into());
        binds.push(SqlSearchBind::Text(format!("{}/%", escape_like(folder))));
    }
    // SQLite cannot perform the evaluator's Unicode normalization. Keep
    // non-ASCII catalogue paths in the candidate set for Rust to decide.
    paths.push(non_ascii_value("sm.local_path"));
    let path_predicate = join_or(paths);
    (
        format!(
            "EXISTS (SELECT 1 \
             FROM message_mailbox_memberships mm \
             JOIN selectable_mailboxes sm ON sm.id = mm.mailbox_id AND sm.account_id = mm.account_id \
             JOIN messages logical ON logical.id = mm.message_id AND logical.account_id = m.account_id \
             WHERE mm.account_id = m.account_id \
               AND (logical.id = m.id OR (m.message_id IS NOT NULL AND logical.message_id IS NOT NULL \
                    AND lower(trim(logical.message_id)) = lower(trim(m.message_id)))) \
               AND ({path_predicate}))"
        ),
        binds,
    )
}

fn date_predicate(
    comparison: DateComparison,
    value: &SearchDate,
    today: NaiveDate,
) -> Option<CompiledNode> {
    let date = resolve_date(value, today)?;
    let operator = match comparison {
        DateComparison::On => "=",
        DateComparison::Before => "<",
        DateComparison::After => ">",
    };
    Some(CompiledNode {
        predicate: Some(format!("date(m.received_at) {operator} date(?)")),
        binds: vec![SqlSearchBind::Text(date.format("%F").to_string())],
        omitted: false,
    })
}

fn attachment_predicate(predicate: AttachmentPredicate) -> Option<CompiledNode> {
    let predicate = match predicate {
        AttachmentPredicate::HasAttachment => {
            "(m.has_attachments = 1 OR EXISTS (SELECT 1 FROM message_attachment_catalogue ac WHERE ac.message_id = m.id AND (ac.presentation IN ('downloadable', 'both') OR (ac.presentation = 'unknown' AND ac.is_inline = 0))))"
        }
        AttachmentPredicate::HasNoAttachment => {
            // A legacy `has_attachments` flag does not tell us whether a
            // named CID image was embedded-only. It can only exclude a row
            // with no catalogue metadata at all, because the evaluator will
            // represent that as an unknown attachment. Known presentation
            // rows let SQLite narrow safely while Rust remains authoritative.
            "(NOT EXISTS (SELECT 1 FROM message_attachment_catalogue ac WHERE ac.message_id = m.id AND (ac.presentation IN ('downloadable', 'both') OR (ac.presentation = 'unknown' AND ac.is_inline = 0))) AND NOT (m.has_attachments = 1 AND NOT EXISTS (SELECT 1 FROM message_attachment_catalogue ac WHERE ac.message_id = m.id)))"
        }
    };
    Some(CompiledNode {
        predicate: Some(predicate.into()),
        binds: Vec::new(),
        // The catalogue is incremental and legacy unknown rows lack a
        // selected-HTML disposition. Canonical evaluation remains final.
        omitted: true,
    })
}

fn filename_predicate(text: &SearchText) -> Option<CompiledNode> {
    let tokens = ascii_tokens(text)?;
    let mut predicates: Vec<String> = Vec::new();
    let mut binds = Vec::new();
    for token in tokens {
        // Substring matching is deliberately broader than canonical token or
        // prefix matching, and therefore safe as a candidate filter.
        predicates.push("lower(ac.filename) LIKE ? ESCAPE '\\'".into());
        binds.push(SqlSearchBind::Text(format!("%{}%", escape_like(&token))));
    }
    Some(CompiledNode {
        predicate: Some(format!(
            "EXISTS (SELECT 1 FROM message_attachment_catalogue ac WHERE ac.message_id = m.id AND ac.presentation IN ('downloadable', 'both') AND (({}) OR {}))",
            predicates.join(" AND "),
            non_ascii_value("ac.filename"),
        )),
        binds,
        omitted: true,
    })
}

fn file_type_predicate(file_type: FileType) -> Option<CompiledNode> {
    let predicate = match file_type {
        FileType::Pdf => "(lower(ac.mime_type) = 'application/pdf' OR lower(ac.filename) LIKE '%.pdf')",
        FileType::Document => "((lower(ac.mime_type) LIKE 'text/%' AND lower(ac.mime_type) != 'text/calendar') OR lower(ac.mime_type) LIKE '%word%' OR lower(ac.filename) LIKE '%.doc' OR lower(ac.filename) LIKE '%.docx' OR lower(ac.filename) LIKE '%.odt' OR lower(ac.filename) LIKE '%.rtf' OR lower(ac.filename) LIKE '%.txt')",
        FileType::Spreadsheet => "(lower(ac.mime_type) LIKE '%spreadsheet%' OR lower(ac.mime_type) LIKE '%excel%' OR lower(ac.filename) LIKE '%.xls' OR lower(ac.filename) LIKE '%.xlsx' OR lower(ac.filename) LIKE '%.ods' OR lower(ac.filename) LIKE '%.csv')",
        FileType::Presentation => "(lower(ac.mime_type) LIKE '%presentation%' OR lower(ac.mime_type) LIKE '%powerpoint%' OR lower(ac.filename) LIKE '%.ppt' OR lower(ac.filename) LIKE '%.pptx' OR lower(ac.filename) LIKE '%.odp')",
        FileType::Image => "(lower(ac.mime_type) LIKE 'image/%' OR lower(ac.filename) LIKE '%.jpg' OR lower(ac.filename) LIKE '%.jpeg' OR lower(ac.filename) LIKE '%.png' OR lower(ac.filename) LIKE '%.gif' OR lower(ac.filename) LIKE '%.webp' OR lower(ac.filename) LIKE '%.heic' OR lower(ac.filename) LIKE '%.svg')",
        FileType::Audio => "(lower(ac.mime_type) LIKE 'audio/%' OR lower(ac.filename) LIKE '%.mp3' OR lower(ac.filename) LIKE '%.wav' OR lower(ac.filename) LIKE '%.m4a' OR lower(ac.filename) LIKE '%.ogg' OR lower(ac.filename) LIKE '%.flac')",
        FileType::Video => "(lower(ac.mime_type) LIKE 'video/%' OR lower(ac.filename) LIKE '%.mp4' OR lower(ac.filename) LIKE '%.mov' OR lower(ac.filename) LIKE '%.mkv' OR lower(ac.filename) LIKE '%.webm' OR lower(ac.filename) LIKE '%.avi')",
        FileType::Archive => "(lower(ac.mime_type) LIKE '%zip%' OR lower(ac.mime_type) LIKE '%compressed%' OR lower(ac.filename) LIKE '%.zip' OR lower(ac.filename) LIKE '%.tar' OR lower(ac.filename) LIKE '%.gz' OR lower(ac.filename) LIKE '%.bz2' OR lower(ac.filename) LIKE '%.xz' OR lower(ac.filename) LIKE '%.7z' OR lower(ac.filename) LIKE '%.rar')",
        FileType::Calendar => "(lower(ac.mime_type) = 'text/calendar' OR lower(ac.filename) LIKE '%.ics' OR lower(ac.filename) LIKE '%.ical')",
    };
    Some(CompiledNode {
        predicate: Some(format!(
            "EXISTS (SELECT 1 FROM message_attachment_catalogue ac WHERE ac.message_id = m.id AND ac.presentation IN ('downloadable', 'both') AND {predicate})"
        )),
        binds: Vec::new(),
        omitted: true,
    })
}

fn fts_query(text: &SearchText) -> Option<String> {
    let tokens = ascii_tokens(text)?;
    if text.quoted {
        let phrase = tokens
            .iter()
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" ");
        return Some(if text.prefix {
            format!("{phrase}*")
        } else {
            phrase
        });
    }

    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(index, token)| {
                if text.prefix && index + 1 == tokens.len() {
                    format!("\"{token}\"*")
                } else {
                    format!("\"{token}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

fn ascii_tokens(text: &SearchText) -> Option<Vec<String>> {
    if !text.value.is_ascii() {
        return None;
    }
    let tokens = text
        .value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then_some(tokens)
}

/// Broad ASCII matching is used only for body tables that do not have an FTS
/// index. It intentionally accepts substrings and non-adjacent phrase words,
/// because canonical evaluation determines the actual match later.
fn ascii_like_all(column: &str, text: &SearchText) -> Option<(String, Vec<SqlSearchBind>)> {
    let tokens = ascii_tokens(text)?;
    let mut predicates = Vec::with_capacity(tokens.len());
    let mut binds = Vec::with_capacity(tokens.len());
    for token in tokens {
        predicates.push(format!("lower(COALESCE({column}, '')) LIKE ? ESCAPE '\\'"));
        binds.push(SqlSearchBind::Text(format!("%{}%", escape_like(&token))));
    }
    Some((predicates.join(" AND "), binds))
}

/// SQLite's built-in case conversion is ASCII-only. Any stored non-ASCII
/// value is deliberately handed to the canonical Rust evaluator, which owns
/// diacritic removal and the few full-case-fold mappings we support.
fn non_ascii_value(column: &str) -> String {
    format!("length(COALESCE({column}, '')) != length(CAST(COALESCE({column}, '') AS BLOB))")
}

fn non_ascii_any(columns: &[&str]) -> String {
    join_or(
        columns
            .iter()
            .map(|column| non_ascii_value(column))
            .collect(),
    )
}

fn resolve_date(value: &SearchDate, today: NaiveDate) -> Option<NaiveDate> {
    match value {
        SearchDate::Absolute(date) => Some(*date),
        SearchDate::Relative { amount, unit } => match unit {
            RelativeDateUnit::Days => today.checked_sub_signed(Duration::days(i64::from(*amount))),
            RelativeDateUnit::Weeks => {
                today.checked_sub_signed(Duration::weeks(i64::from(*amount)))
            }
            RelativeDateUnit::Months => subtract_months(today, *amount),
            RelativeDateUnit::Years => subtract_months(today, amount.checked_mul(12)?),
        },
    }
}

fn subtract_months(date: NaiveDate, amount: u32) -> Option<NaiveDate> {
    let month_offset = i64::from(date.year()) * 12 + i64::from(date.month0()) - i64::from(amount);
    let year = i32::try_from(month_offset.div_euclid(12)).ok()?;
    let month = u32::try_from(month_offset.rem_euclid(12)).ok()? + 1;
    let last_day = (28..=31)
        .rev()
        .find(|day| NaiveDate::from_ymd_opt(year, month, *day).is_some())?;
    NaiveDate::from_ymd_opt(year, month, date.day().min(last_day))
}

fn join_or(predicates: Vec<String>) -> String {
    predicates
        .into_iter()
        .map(wrap)
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn wrap(predicate: String) -> String {
    format!("({predicate})")
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn special_mailbox_family(folder: &str) -> bool {
    matches!(
        folder.to_ascii_lowercase().as_str(),
        "sent" | "drafts" | "archive" | "spam" | "trash"
    )
}

fn validate_node(node: &SearchNode) -> Result<(), SqlSearchCompileError> {
    match node {
        SearchNode::MatchAll => Ok(()),
        SearchNode::Term(term) => validate_term(term),
        SearchNode::And(nodes) | SearchNode::Or(nodes) => {
            for node in nodes {
                validate_node(node)?;
            }
            Ok(())
        }
        SearchNode::Not(node) => validate_node(node),
    }
}

fn validate_term(term: &SearchTerm) -> Result<(), SqlSearchCompileError> {
    let literals: Vec<&str> = match term {
        SearchTerm::Text(text) | SearchTerm::Filename(text) => vec![&text.value],
        SearchTerm::Field { value, .. } => vec![&value.value],
        SearchTerm::Folder(FolderScope::Exact(folder) | FolderScope::Descendants(folder)) => {
            vec![folder]
        }
        SearchTerm::Folder(FolderScope::All)
        | SearchTerm::Date { .. }
        | SearchTerm::Attachment(_)
        | SearchTerm::FileType(_)
        | SearchTerm::State(_) => Vec::new(),
    };
    if literals.iter().any(|literal| {
        literal
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
    }) {
        return Err(SqlSearchCompileError {
            kind: SqlSearchCompileErrorKind::ControlCharacter,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{parse_search_query, SearchExpression, SearchNode, SearchTerm, SearchText};
    use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};

    fn compile(query: &str) -> SqlSearchCandidate {
        compile_sql_candidate(
            &parse_search_query(query).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        )
        .unwrap()
    }

    async fn matching_mailboxes(pool: &SqlitePool, query: &str) -> Vec<String> {
        let candidate = compile(query);
        let sql = format!(
            "SELECT m.mailbox FROM messages m WHERE {} ORDER BY m.mailbox",
            candidate.predicate
        );
        let mut statement = sqlx::query_scalar::<_, String>(&sql);
        for bind in candidate.binds {
            statement = match bind {
                SqlSearchBind::Text(value) => statement.bind(value),
                SqlSearchBind::Integer(value) => statement.bind(value),
            };
        }
        statement.fetch_all(pool).await.unwrap()
    }

    #[test]
    fn binds_fts_text_instead_of_interpolating_it() {
        let expression = SearchExpression {
            root: SearchNode::Term(SearchTerm::Field {
                field: SearchField::Subject,
                value: SearchText {
                    value: "invoice' OR 1 = 1 --".into(),
                    quoted: false,
                    prefix: false,
                },
            }),
        };
        let compiled =
            compile_sql_candidate(&expression, NaiveDate::from_ymd_opt(2026, 9, 8).unwrap())
                .unwrap();
        assert!(!compiled.predicate.contains("invoice"));
        assert!(compiled.predicate.contains("MATCH ?"));
        assert_eq!(compiled.binds.len(), 1);
        assert!(
            matches!(&compiled.binds[0], SqlSearchBind::Text(value) if value.contains("invoice"))
        );
    }

    #[test]
    fn binds_folder_and_escapes_like_wildcards() {
        let expression = SearchExpression {
            root: SearchNode::Term(SearchTerm::Folder(FolderScope::Descendants(
                "Projects_%\\2026".into(),
            ))),
        };
        let compiled =
            compile_sql_candidate(&expression, NaiveDate::from_ymd_opt(2026, 9, 8).unwrap())
                .unwrap();
        assert!(!compiled.predicate.contains("Projects"));
        assert_eq!(
            compiled.binds,
            vec![
                SqlSearchBind::Text("Projects_%\\2026".into()),
                SqlSearchBind::Text("Projects\\_\\%\\\\2026/%".into()),
                SqlSearchBind::Text("Projects_%\\2026".into()),
                SqlSearchBind::Text("Projects\\_\\%\\\\2026/%".into()),
                SqlSearchBind::Text("%::Projects\\_\\%\\\\2026".into()),
                SqlSearchBind::Text("%::Projects\\_\\%\\\\2026/%".into()),
            ]
        );
        assert!(compiled
            .predicate
            .contains("message_mailbox_memberships mm"));
        assert!(compiled
            .predicate
            .contains("length(COALESCE(m.mailbox, ''))"));
    }

    #[test]
    fn preserves_safe_and_prefilters_when_or_or_not_cannot() {
        let and = compile("is:unread AND subject:invoice");
        assert!(and.predicate.contains("m.is_read = 0"));
        assert!(and.predicate.contains("MATCH ?"));
        assert!(and.requires_post_filter);

        let or = compile("is:unread OR subject:Åsa");
        assert_eq!(or.predicate, "1 = 1");
        assert!(or.requires_post_filter);

        let indexed_or = compile("is:unread OR subject:invoice");
        assert_eq!(indexed_or.predicate, "1 = 1");
        assert!(indexed_or.requires_post_filter);

        let not = compile("NOT subject:Åsa");
        assert_eq!(not.predicate, "1 = 1");
        assert!(not.requires_post_filter);
    }

    #[test]
    fn compiles_common_catalogue_predicates() {
        let compiled = compile(
            "from:alice after:2d in:Projects/* has:attachment filename:invoice* filetype:pdf is:answered",
        );
        assert!(compiled.binds.iter().any(
            |bind| matches!(bind, SqlSearchBind::Text(value) if value == "from_name : \"alice\"")
        ));
        assert!(compiled.predicate.contains("date(m.received_at) > date(?)"));
        assert!(compiled.predicate.contains("m.mailbox LIKE ?"));
        assert!(compiled
            .predicate
            .contains("message_attachment_catalogue ac"));
        assert!(compiled.predicate.contains("m.is_answered = 1"));
        assert!(compiled.requires_post_filter);
    }

    #[test]
    fn people_fields_preserve_fastmail_to_and_tonotcc_scope() {
        let to = compile("to:alice");
        for column in ["to_addresses", "cc_addresses", "bcc_addresses"] {
            assert!(
                to.binds.iter().any(|bind| {
                    matches!(bind, SqlSearchBind::Text(value) if value == &format!("{column} : \"alice\""))
                }),
                "to: must include {column}"
            );
        }

        let to_not_cc = compile("tonotcc:alice");
        assert_eq!(
            to_not_cc.binds,
            vec![SqlSearchBind::Text("to_addresses : \"alice\"".into())]
        );
        assert!(!to_not_cc.predicate.contains("NOT IN"));
    }

    #[test]
    fn rejects_constructed_control_character_ast() {
        let expression = SearchExpression {
            root: SearchNode::Term(SearchTerm::Text(SearchText {
                value: "invoice\r\nOR 1=1".into(),
                quoted: false,
                prefix: false,
            })),
        };
        assert!(matches!(
            compile_sql_candidate(&expression, NaiveDate::from_ymd_opt(2026, 9, 8).unwrap()),
            Err(SqlSearchCompileError {
                kind: SqlSearchCompileErrorKind::ControlCharacter
            })
        ));
    }

    #[test]
    fn unicode_uses_evaluator_fallback_not_ascii_collation() {
        let compiled = compile("subject:résumé in:Århus");
        assert_eq!(compiled.predicate, "1 = 1");
        assert!(compiled.requires_post_filter);
    }

    #[test]
    fn keeps_non_ascii_rows_for_canonical_unicode_folding() {
        // The canonical evaluator treats `strasse` and `Straße` alike, while
        // SQLite's unicode61 tokenizer does not promise that full fold.
        let subject = compile("subject:strasse");
        assert!(subject.predicate.contains("COALESCE(m.subject, '')"));

        // An ASCII query may match a diacritic-bearing folder after canonical
        // normalization, so an SQL equality candidate alone is not enough.
        let folder = compile("in:arhus");
        assert!(folder.predicate.contains("COALESCE(m.mailbox, '')"));

        // Filename matching has the same Unicode normalization requirement.
        let filename = compile("filename:invoice");
        assert!(filename.predicate.contains("COALESCE(ac.filename, '')"));
    }

    #[test]
    fn negated_folder_does_not_negate_the_unicode_candidate_fallback() {
        // SQLite NOCASE cannot decide whether Århus is `Archive`; the
        // positive folder compiler intentionally keeps Unicode rows for the
        // canonical evaluator. Negating that superset would incorrectly lose
        // every non-ASCII mailbox.
        let compiled = compile("NOT in:Archive");
        assert_eq!(compiled.predicate, "1 = 1");
        assert!(compiled.requires_post_filter);

        let mixed = compile("NOT in:Archive AND is:unread");
        assert_eq!(mixed.predicate, "(m.is_read = 0)");
        assert!(mixed.requires_post_filter);
    }

    #[test]
    fn special_mailbox_families_include_internal_provider_aliases() {
        for folder in ["Sent", "Drafts", "Archive", "Spam", "Trash"] {
            let exact = compile(&format!("in:{folder}"));
            assert!(exact.predicate.contains("m.mailbox LIKE ?"), "{folder}");
            assert!(exact
                .binds
                .contains(&SqlSearchBind::Text(format!("{folder}::%"))));

            let descendants = compile(&format!("in:{folder}/*"));
            assert!(
                descendants
                    .binds
                    .contains(&SqlSearchBind::Text(format!("{folder}/%"))),
                "{folder} descendants preserve visible slash paths"
            );
            assert!(
                descendants
                    .binds
                    .contains(&SqlSearchBind::Text(format!("%::{folder}"))),
                "{folder} descendants include a visible alias of the same name"
            );
        }
    }

    #[test]
    fn visible_remote_aliases_match_opaque_special_storage_ids() {
        let exact = compile("in:\"[Gmail]/Drafts\"");
        assert!(exact.predicate.contains("m.mailbox LIKE ?"));
        assert!(exact
            .binds
            .contains(&SqlSearchBind::Text("%::[Gmail]/Drafts".into())));
        assert!(!exact.predicate.contains("[Gmail]/Drafts"));

        let descendants = compile("in:\"[Gmail]/*\"");
        assert!(descendants
            .binds
            .contains(&SqlSearchBind::Text("%::[Gmail]".into())));
        assert!(descendants
            .binds
            .contains(&SqlSearchBind::Text("%::[Gmail]/%".into())));
    }

    #[tokio::test]
    async fn gmail_visible_path_candidate_matches_the_storage_shape() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, message_id TEXT, mailbox TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE selectable_mailboxes (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, local_path TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE message_mailbox_memberships (message_id TEXT NOT NULL, mailbox_id TEXT NOT NULL, account_id TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(id, account_id, message_id, mailbox) VALUES ('draft', 'account', NULL, 'Drafts::[Gmail]/Drafts')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(id, account_id, message_id, mailbox) VALUES ('spam', 'account', NULL, 'Spam::Bulk')")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            matching_mailboxes(&pool, "in:\"[Gmail]/Drafts\"").await,
            vec!["Drafts::[Gmail]/Drafts"]
        );
        assert_eq!(
            matching_mailboxes(&pool, "in:\"[Gmail]/*\"").await,
            vec!["Drafts::[Gmail]/Drafts"]
        );
        assert!(matching_mailboxes(&pool, "in:Drafts/*").await.is_empty());
        assert_eq!(
            matching_mailboxes(&pool, "in:\"Bulk\"").await,
            vec!["Spam::Bulk"]
        );
    }

    #[tokio::test]
    async fn opaque_provider_mailbox_locators_remain_search_candidates() {
        use crate::search_eval::{
            generic_mailbox_storage_identity, special_mailbox_storage_identity,
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, message_id TEXT, mailbox TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE selectable_mailboxes (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, local_path TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE message_mailbox_memberships (message_id TEXT NOT NULL, mailbox_id TEXT NOT NULL, account_id TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        let generic = generic_mailbox_storage_identity("Projects.Client", "Projects/Client");
        let special = special_mailbox_storage_identity("Sent", "Sent Items");
        sqlx::query("INSERT INTO messages(id, account_id, message_id, mailbox) VALUES ('generic', 'account', NULL, ?), ('special', 'account', NULL, ?)")
            .bind(&generic)
            .bind(&special)
            .execute(&pool)
            .await
            .unwrap();

        // SQL cannot inspect the encoded display path. It deliberately keeps
        // opaque rows for the canonical evaluator, which resolves the exact
        // visible folder semantics afterward.
        assert_eq!(
            matching_mailboxes(&pool, "in:\"Projects/Client\"").await,
            vec![generic.clone(), special.clone()]
        );
        assert_eq!(
            matching_mailboxes(&pool, "in:\"Sent Items\"").await,
            vec![generic, special]
        );
    }

    #[tokio::test]
    async fn folder_candidates_join_logical_rfc_message_id_alias_memberships() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, message_id TEXT, mailbox TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE selectable_mailboxes (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, local_path TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE message_mailbox_memberships (message_id TEXT NOT NULL, mailbox_id TEXT NOT NULL, account_id TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(id, account_id, message_id, mailbox) VALUES ('inbox-copy', 'account', ' <same@example.test> ', 'INBOX'), ('archive-copy', 'account', '<SAME@example.test>', 'Archive')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO selectable_mailboxes(id, account_id, local_path) VALUES ('clients', 'account', 'Clients'), ('invoices', 'account', 'Invoices')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO message_mailbox_memberships(message_id, mailbox_id, account_id) VALUES ('inbox-copy', 'clients', 'account'), ('archive-copy', 'invoices', 'account')")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            matching_mailboxes(&pool, "in:Clients in:Invoices").await,
            vec!["Archive", "INBOX"],
            "each physical row sees the logical message's union of mailbox memberships"
        );
    }

    #[test]
    fn covers_legacy_attachment_and_all_body_sources() {
        let attachment = compile("has:attachment");
        assert!(attachment.predicate.contains("m.has_attachments = 1"));
        assert!(attachment
            .predicate
            .contains("message_attachment_catalogue ac"));
        assert!(attachment
            .predicate
            .contains("ac.presentation IN ('downloadable', 'both')"));

        // Inline CIDs and legacy unknown metadata have a precise conservative
        // candidate rule. Rust still performs canonical evaluation.
        let no_attachment = compile("has:noattachment");
        assert!(no_attachment
            .predicate
            .contains("ac.presentation IN ('downloadable', 'both')"));

        let filename = compile("filename:invoice*");
        assert!(filename
            .predicate
            .contains("ac.presentation IN ('downloadable', 'both')"));
        let file_type = compile("filetype:image");
        assert!(file_type
            .predicate
            .contains("ac.presentation IN ('downloadable', 'both')"));

        let body = compile("body:invoice");
        assert!(body.predicate.contains("message_cached_bodies_fts"));
        assert!(body.predicate.contains("message_search_bodies_fts"));
        assert!(body.predicate.contains("starred_message_bodies"));
        assert!(body.predicate.contains("m.body_text"));
    }

    #[test]
    fn file_type_predicates_cover_every_evaluator_alias() {
        let video = compile("filetype:video");
        assert!(video.predicate.contains("%.avi"));

        let archive = compile("filetype:archive");
        assert!(archive.predicate.contains("%compressed%"));
        assert!(archive.predicate.contains("%.bz2"));
        assert!(archive.predicate.contains("%.xz"));
    }

    #[test]
    fn phrase_and_prefix_generate_fts_literals() {
        let phrase = compile("subject:\"project alpha\"");
        assert_eq!(
            phrase.binds,
            vec![SqlSearchBind::Text(
                "subject : \"project\" \"alpha\"".into()
            )]
        );
        let prefix = compile("subject:proj*");
        assert_eq!(
            prefix.binds,
            vec![SqlSearchBind::Text("subject : \"proj\"*".into())]
        );
    }
}
