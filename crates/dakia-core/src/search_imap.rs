//! Conservative generic IMAP SEARCH candidate compiler.
//!
//! The output is deliberately a superset candidate query. Any predicate that
//! cannot be represented without risking false negatives is omitted and must
//! be checked by the canonical evaluator before a result is shown.

use crate::search::{
    FolderScope, MessageState, SearchExpression, SearchField, SearchNode, SearchTerm, SearchText,
};
use chrono::NaiveDate;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapSearchCandidate {
    /// IMAP SEARCH criteria only, without a command tag or `UID SEARCH`.
    pub criteria: String,
    /// True when the candidate query intentionally omitted at least one AST
    /// predicate and therefore requires canonical post-filtering.
    pub requires_post_filter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapCompileError {
    pub kind: ImapCompileErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImapCompileErrorKind {
    ControlCharacter,
}

impl fmt::Display for ImapCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ImapCompileErrorKind::ControlCharacter => {
                write!(
                    formatter,
                    "IMAP search literals cannot contain control characters"
                )
            }
        }
    }
}

impl std::error::Error for ImapCompileError {}

/// Compile a parsed expression into generic IMAP SEARCH criteria. `today` is
/// required to resolve relative date terms deterministically.
pub fn compile_generic_imap(
    expression: &SearchExpression,
    today: NaiveDate,
) -> Result<ImapSearchCandidate, ImapCompileError> {
    validate_node(&expression.root)?;
    let compiled = compile_node(&expression.root, today)?;
    Ok(ImapSearchCandidate {
        criteria: compiled.criteria.unwrap_or_else(|| "ALL".into()),
        requires_post_filter: compiled.omitted,
    })
}

struct CompiledNode {
    criteria: Option<String>,
    omitted: bool,
}

fn compile_node(node: &SearchNode, today: NaiveDate) -> Result<CompiledNode, ImapCompileError> {
    match node {
        SearchNode::MatchAll => Ok(CompiledNode {
            criteria: Some("ALL".into()),
            omitted: false,
        }),
        SearchNode::Term(term) => compile_term(term, today),
        SearchNode::And(nodes) => {
            let mut criteria = Vec::new();
            let mut omitted = false;
            for node in nodes {
                let child = compile_node(node, today)?;
                omitted |= child.omitted || child.criteria.is_none();
                if let Some(child) = child.criteria {
                    criteria.push(wrap(child));
                }
            }
            Ok(CompiledNode {
                criteria: (!criteria.is_empty()).then(|| criteria.join(" ")),
                omitted,
            })
        }
        SearchNode::Or(nodes) => {
            let mut criteria = Vec::new();
            let mut omitted = false;
            for node in nodes {
                let child = compile_node(node, today)?;
                // A partial OR branch represents an unknown set of canonical
                // matches. Keeping its provider criterion would omit rows
                // that match only the deferred portion (for example Bcc-only
                // `to:`), so the whole OR must widen to ALL.
                if child.omitted || child.criteria.is_none() {
                    return Ok(CompiledNode {
                        criteria: None,
                        omitted: true,
                    });
                }
                omitted |= child.omitted;
                let Some(child) = child.criteria else {
                    // Dropping a branch from OR could discard real matches.
                    return Ok(CompiledNode {
                        criteria: None,
                        omitted: true,
                    });
                };
                criteria.push(child);
            }
            Ok(CompiledNode {
                criteria: or_criteria(&criteria),
                omitted,
            })
        }
        SearchNode::Not(node) => {
            let child = compile_node(node, today)?;
            match child.criteria {
                // IMAP text keys are substring predicates while Dakia text
                // predicates are complete tokens. Their positive result is a
                // useful superset, but negating it would exclude a canonical
                // match such as `NOT subject:cat` for `catalog`.
                Some(criteria) if !child.omitted && inversion_is_safe(node) => Ok(CompiledNode {
                    criteria: Some(format!("NOT {}", wrap(criteria))),
                    omitted: false,
                }),
                _ => Ok(CompiledNode {
                    criteria: None,
                    omitted: true,
                }),
            }
        }
    }
}

fn compile_term(term: &SearchTerm, _today: NaiveDate) -> Result<CompiledNode, ImapCompileError> {
    let (criteria, omitted) = match term {
        SearchTerm::Text(text) => safe_text_key("TEXT", text),
        SearchTerm::Field { field, value } => match field {
            SearchField::From => safe_text_key("FROM", value),
            // Fastmail `to:` checks To, Cc, and Bcc. A generic IMAP server
            // does not reliably expose Bcc (in particular for Sent copies),
            // so narrowing to TO/CC would discard Bcc-only canonical matches.
            // Leave the whole recipient-wide predicate to post-filtering.
            SearchField::To => {
                return Ok(CompiledNode {
                    criteria: None,
                    omitted: true,
                })
            }
            // `tonotcc:` is a canonical To-only complete-word predicate. A
            // positive TO substring candidate is a safe superset; Cc is not a
            // negative condition and must not affect this result.
            SearchField::ToNotCc => {
                return Ok(CompiledNode {
                    criteria: safe_text_key("TO", value),
                    omitted: true,
                })
            }
            SearchField::Cc => safe_text_key("CC", value),
            SearchField::Bcc => safe_text_key("BCC", value),
            SearchField::With => {
                let keys = ["FROM", "TO", "CC", "BCC"]
                    .into_iter()
                    .map(|key| safe_text_key(key, value))
                    .collect::<Option<Vec<_>>>();
                keys.as_deref().and_then(or_criteria)
            }
            SearchField::Subject => safe_text_key("SUBJECT", value),
            SearchField::Body => safe_text_key("BODY", value),
        },
        // Generic IMAP ON/BEFORE/SINCE use INTERNALDATE. Dakia's canonical
        // semantics use the Date header, so none of these can narrow a
        // provider candidate safely.
        SearchTerm::Date { .. } => None,
        SearchTerm::State(state) => Some(state_criteria(*state)),
        // Generic IMAP SEARCH runs within a selected mailbox and has no safe,
        // standard attachment-metadata or cross-folder primitive.
        SearchTerm::Folder(
            FolderScope::Exact(_) | FolderScope::Descendants(_) | FolderScope::All,
        )
        | SearchTerm::Attachment(_)
        | SearchTerm::Filename(_)
        | SearchTerm::FileType(_) => None,
    }
    .map_or((None, true), |criteria| (Some(criteria), false));
    Ok(CompiledNode { omitted, criteria })
}

/// A negated IMAP criterion is sound only where IMAP and canonical Dakia
/// predicate semantics are identical. Positive substring terms are deliberately
/// excluded here even when they are useful candidate filters on their own.
fn inversion_is_safe(node: &SearchNode) -> bool {
    match node {
        SearchNode::MatchAll => true,
        SearchNode::Term(SearchTerm::State(_)) => true,
        SearchNode::And(nodes) | SearchNode::Or(nodes) => nodes.iter().all(inversion_is_safe),
        SearchNode::Not(node) => inversion_is_safe(node),
        SearchNode::Term(_) => false,
    }
}

fn safe_text_key(key: &str, text: &SearchText) -> Option<String> {
    // IMAP's substring search is a safe superset for a single ASCII canonical
    // token. More complex Unicode, phrase, and punctuation matching is left to
    // the local evaluator so it cannot become a false negative.
    if text.quoted
        || text.value.is_empty()
        || !text.value.is_ascii()
        || !text.value.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!("{key} {}", quote(&text.value)))
}

fn state_criteria(state: MessageState) -> String {
    match state {
        MessageState::Read => "SEEN",
        MessageState::Unread => "UNSEEN",
        MessageState::Flagged => "FLAGGED",
        MessageState::Unflagged => "UNFLAGGED",
        MessageState::Replied => "ANSWERED",
        MessageState::Unreplied => "UNANSWERED",
        MessageState::Draft => "DRAFT",
        MessageState::Undraft => "UNDRAFT",
    }
    .into()
}

fn or_criteria(criteria: &[String]) -> Option<String> {
    let mut iter = criteria.iter();
    let first = iter.next()?.clone();
    Some(iter.fold(first, |left, right| {
        format!("OR {} {}", wrap(left), wrap(right.clone()))
    }))
}

fn wrap(criteria: String) -> String {
    format!("({criteria})")
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn validate_node(node: &SearchNode) -> Result<(), ImapCompileError> {
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

fn validate_term(term: &SearchTerm) -> Result<(), ImapCompileError> {
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
        return Err(ImapCompileError {
            kind: ImapCompileErrorKind::ControlCharacter,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{parse_search_query, SearchExpression, SearchNode, SearchTerm, SearchText};

    fn compile(query: &str) -> ImapSearchCandidate {
        compile_generic_imap(
            &parse_search_query(query).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn compiles_precedence_as_explicit_imap_grouping() {
        let compiled = compile("from:alice OR to:bob AND is:unread");
        assert_eq!(compiled.criteria, "ALL");
        assert!(compiled.requires_post_filter);
    }

    #[test]
    fn recipient_wide_to_never_excludes_a_bcc_only_match() {
        // BCC is a standard SEARCH key, but it is not dependable for a
        // generic provider's stored Sent copy. `to:` has Bcc semantics, so a
        // TO/CC candidate query is unsound. The canonical evaluator receives
        // all candidates and can inspect the Bcc header when it is available.
        let compiled = compile("to:hidden@example.test AND is:unread");
        assert_eq!(compiled.criteria, "(UNSEEN)");
        assert!(compiled.requires_post_filter);

        let or = compile("to:hidden@example.test OR is:unread");
        assert_eq!(or.criteria, "ALL");
        assert!(or.requires_post_filter);
    }

    #[test]
    fn compiles_standard_keys_and_omits_unsafe_narrowing() {
        let compiled = compile("subject:invoice after:2d has:attachment filename:invoice*");
        assert_eq!(compiled.criteria, "(SUBJECT \"invoice\")");
        assert!(compiled.requires_post_filter);
    }

    #[test]
    fn unsupported_or_and_not_branches_become_all_candidates() {
        let or = compile("subject:invoice OR filename:invoice");
        assert_eq!(or.criteria, "ALL");
        assert!(or.requires_post_filter);
        let not = compile("NOT filename:invoice");
        assert_eq!(not.criteria, "ALL");
        assert!(not.requires_post_filter);
    }

    #[test]
    fn negated_substring_text_cannot_narrow_complete_word_semantics() {
        // Dakia's `subject:cat` does not match the complete word `catalog`,
        // while generic IMAP SUBJECT does. `NOT SUBJECT \"cat\"` would drop a
        // canonical `NOT subject:cat` result, so the provider must see ALL.
        let compiled = compile("NOT subject:cat");
        assert_eq!(compiled.criteria, "ALL");
        assert!(compiled.requires_post_filter);

        // Standard flag keys have the same boolean meaning as canonical state.
        let state = compile("NOT is:read");
        assert_eq!(state.criteria, "NOT (SEEN)");
        assert!(!state.requires_post_filter);
    }

    #[test]
    fn tonotcc_uses_only_the_safe_positive_to_superset() {
        // CC is a substring predicate. Negating it can exclude a canonical
        // complete-word result, so the CC clause stays in the post-filter.
        let compiled = compile("tonotcc:cat");
        assert_eq!(compiled.criteria, "TO \"cat\"");
        assert!(compiled.requires_post_filter);
        let negated = compile("NOT tonotcc:cat");
        assert_eq!(negated.criteria, "ALL");
        assert!(negated.requires_post_filter);

        // A partial branch in OR represents an unknown candidate set, so it
        // widens to ALL rather than dropping canonical matches.
        let in_or = compile("tonotcc:cat OR is:unread");
        assert_eq!(in_or.criteria, "ALL");
        assert!(in_or.requires_post_filter);
    }

    #[test]
    fn date_header_predicates_never_use_imap_internaldate_candidates() {
        // A message can have Date: 2026-09-06 while an append/import gives it
        // a later INTERNALDATE. Date constraints remain canonical-only.
        for query in ["date:2026-09-06", "before:2026-09-07", "after:2d"] {
            let compiled = compile(query);
            assert_eq!(compiled.criteria, "ALL", "{query}");
            assert!(compiled.requires_post_filter, "{query}");
        }
        let in_or = compile("subject:invoice OR date:2026-09-06");
        assert_eq!(in_or.criteria, "ALL");
        assert!(in_or.requires_post_filter);
    }

    #[test]
    fn rejects_constructed_control_character_ast_before_any_output() {
        let expression = SearchExpression {
            root: SearchNode::Term(SearchTerm::Text(SearchText {
                value: "invoice\r\nALL".into(),
                quoted: false,
                prefix: false,
            })),
        };
        assert!(matches!(
            compile_generic_imap(&expression, NaiveDate::from_ymd_opt(2026, 9, 8).unwrap()),
            Err(ImapCompileError {
                kind: ImapCompileErrorKind::ControlCharacter
            })
        ));
    }

    #[test]
    fn quotes_imap_strings_without_leaking_delimiters() {
        assert_eq!(quote("a\\b\"c"), "\"a\\\\b\\\"c\"");
    }
}
