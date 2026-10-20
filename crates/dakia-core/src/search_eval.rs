//! Canonical, provider-independent evaluation of parsed search expressions.

use crate::search::{
    AttachmentPredicate, DateComparison, FileType, FolderScope, MessageState, RelativeDateUnit,
    SearchDate, SearchExpression, SearchField, SearchNode, SearchTerm, SearchText,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use chrono::{Datelike, Duration, NaiveDate};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Attachment metadata that can be searched without reading attachment bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchableAttachment<'a> {
    pub filename: Option<&'a str>,
    pub mime_type: Option<&'a str>,
}

/// The provider-neutral message projection needed to evaluate the supported
/// search language. Missing fields are represented as empty strings.
#[derive(Debug, Clone)]
pub struct SearchableMessage<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub cc: &'a str,
    pub bcc: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    /// Logical mailbox memberships for one message. A provider can expose one
    /// message through several labels or folders; the legacy `mailbox` value
    /// remains part of matching for callers that have not populated this yet.
    pub mailboxes: &'a [&'a str],
    pub mailbox: &'a str,
    pub received_on: NaiveDate,
    pub attachments: &'a [SearchableAttachment<'a>],
    pub is_read: bool,
    pub is_flagged: bool,
    pub is_replied: bool,
    pub is_draft: bool,
}

/// Evaluate an expression using `today` as the reference for relative dates.
/// Supplying the date makes replaying and testing a query deterministic.
pub fn evaluate_search(
    expression: &SearchExpression,
    message: &SearchableMessage<'_>,
    today: NaiveDate,
) -> bool {
    evaluate_node(&expression.root, message, today)
}

/// Alias for `evaluate_search` for consumers that already have an AST node.
pub fn evaluate_search_node(
    node: &SearchNode,
    message: &SearchableMessage<'_>,
    today: NaiveDate,
) -> bool {
    evaluate_node(node, message, today)
}

fn evaluate_node(node: &SearchNode, message: &SearchableMessage<'_>, today: NaiveDate) -> bool {
    match node {
        SearchNode::MatchAll => true,
        SearchNode::Term(term) => evaluate_term(term, message, today),
        SearchNode::And(nodes) => nodes.iter().all(|node| evaluate_node(node, message, today)),
        SearchNode::Or(nodes) => nodes.iter().any(|node| evaluate_node(node, message, today)),
        SearchNode::Not(node) => !evaluate_node(node, message, today),
    }
}

fn evaluate_term(term: &SearchTerm, message: &SearchableMessage<'_>, today: NaiveDate) -> bool {
    match term {
        SearchTerm::Text(query) => [
            message.from,
            message.to,
            message.cc,
            message.bcc,
            message.subject,
            message.body,
        ]
        .into_iter()
        .any(|value| text_matches(value, query)),
        SearchTerm::Field { field, value } => match field {
            SearchField::From => text_matches(message.from, value),
            // Fastmail's `to:` considers every recipient field. `tonotcc:`
            // is intentionally narrower: it means the actual To header,
            // regardless of whether the address is also copied.
            SearchField::To => [message.to, message.cc, message.bcc]
                .into_iter()
                .any(|recipient| text_matches(recipient, value)),
            SearchField::ToNotCc => text_matches(message.to, value),
            SearchField::Cc => text_matches(message.cc, value),
            SearchField::Bcc => text_matches(message.bcc, value),
            SearchField::With => [message.from, message.to, message.cc, message.bcc]
                .into_iter()
                .any(|recipient| text_matches(recipient, value)),
            SearchField::Subject => text_matches(message.subject, value),
            SearchField::Body => text_matches(message.body, value),
        },
        SearchTerm::Folder(scope) => {
            folder_matches(scope, message.mailbox)
                || message
                    .mailboxes
                    .iter()
                    .any(|mailbox| folder_matches(scope, mailbox))
        }
        SearchTerm::Date { comparison, value } => {
            date_matches(*comparison, value, message.received_on, today)
        }
        SearchTerm::Attachment(predicate) => match predicate {
            AttachmentPredicate::HasAttachment => !message.attachments.is_empty(),
            AttachmentPredicate::HasNoAttachment => message.attachments.is_empty(),
        },
        SearchTerm::Filename(query) => message
            .attachments
            .iter()
            .filter_map(|attachment| attachment.filename)
            .any(|filename| text_matches(filename, query)),
        SearchTerm::FileType(file_type) => message
            .attachments
            .iter()
            .any(|attachment| file_type_matches(*file_type, attachment)),
        SearchTerm::State(state) => state_matches(*state, message),
    }
}

/// Stable Unicode case folding used by text and person fields. Compatibility
/// decomposition happens before case folding, so compatibility characters such
/// as the `ﬀ` ligature and long `ſ` compare as ordinary `ff` and `s`. Dropping
/// combining marks preserves Dakia's diacritic-insensitive matching, so `Åsa`
/// and `asa` compare alike.
pub fn normalize_search_text(input: &str) -> String {
    input
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(|character| match character {
            // Rust's lowercase mapping is deliberately not a full case fold.
            // These mappings keep the common multi-character and Greek-final
            // forms stable without making provider behavior part of search.
            'ß' | 'ẞ' => "ss".chars().collect::<Vec<_>>(),
            'ς' => "σ".chars().collect::<Vec<_>>(),
            // These Greek symbol forms have full Unicode case-fold mappings
            // to their ordinary letter forms but are not all lowercased by
            // Rust's scalar lowercase conversion.
            'ϐ' => "β".chars().collect::<Vec<_>>(),
            'ϑ' => "θ".chars().collect::<Vec<_>>(),
            'ϕ' => "φ".chars().collect::<Vec<_>>(),
            'ϖ' => "π".chars().collect::<Vec<_>>(),
            'ϰ' => "κ".chars().collect::<Vec<_>>(),
            'ϱ' => "ρ".chars().collect::<Vec<_>>(),
            'ϵ' => "ε".chars().collect::<Vec<_>>(),
            character => character.to_lowercase().collect(),
        })
        .collect()
}

fn tokens(input: &str) -> Vec<String> {
    normalize_search_text(input)
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn text_matches(value: &str, query: &SearchText) -> bool {
    let needles = tokens(&query.value);
    if needles.is_empty() {
        return false;
    }
    let haystack = tokens(value);
    if query.quoted {
        return haystack
            .windows(needles.len())
            .any(|window| window == needles.as_slice());
    }

    needles.iter().enumerate().all(|(index, needle)| {
        if query.prefix && index + 1 == needles.len() {
            haystack.iter().any(|token| token.starts_with(needle))
        } else {
            haystack.iter().any(|token| token == needle)
        }
    })
}

/// Prefix inside a role-qualified storage locator. It is encoded together
/// with the remote path so a legal provider mailbox named `Sent::Foo` can
/// never collide with a resolved Sent role whose remote alias is `Foo`.
pub const SPECIAL_MAILBOX_LOCATOR_PREFIX: &str = "@dakia-special-v1:";
pub const GENERIC_MAILBOX_LOCATOR_PREFIX: &str = "Mailbox::@dakia-mailbox-v1:";

/// Returns an opaque storage locator for a resolved special-use mailbox. The
/// visible family and provider alias are deliberately distinct from this
/// value: it is used as a database/UID namespace key, never as a mailbox path.
pub fn special_mailbox_storage_identity(family: &str, remote: &str) -> String {
    format!(
        "{family}::{SPECIAL_MAILBOX_LOCATOR_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(remote.as_bytes())
    )
}

/// Returns an opaque storage locator for a non-special provider mailbox. The
/// raw IMAP name is retained for identity, while `display_path` is separately
/// encoded for canonical `in:` matching. This prevents mailbox names that
/// differ only in wire representation, case, whitespace, or modified UTF-7
/// spelling from sharing a UID namespace.
pub fn generic_mailbox_storage_identity(remote: &str, display_path: &str) -> String {
    format!(
        "{GENERIC_MAILBOX_LOCATOR_PREFIX}{}:{}",
        URL_SAFE_NO_PAD.encode(remote.as_bytes()),
        URL_SAFE_NO_PAD.encode(display_path.as_bytes())
    )
}

/// True when a user-controlled visible mailbox path would otherwise imitate a
/// special-use locator. Callers escape this exact sequence in normal paths.
pub fn is_special_mailbox_storage_identity(value: &str) -> bool {
    let Some((family, encoded)) = value.split_once("::") else {
        return false;
    };
    is_special_family(family)
        && encoded
            .strip_prefix(SPECIAL_MAILBOX_LOCATOR_PREFIX)
            .is_some_and(|encoded| URL_SAFE_NO_PAD.decode(encoded).is_ok())
}

/// Decode standard IMAP modified UTF-7 for display and search only. Callers
/// must retain the original wire string for LIST/SELECT and mailbox identity.
/// Invalid sequences remain unchanged rather than being guessed.
pub fn display_imap_mailbox_name(value: &str) -> String {
    let mut output = String::new();
    let mut remaining = value;
    while let Some(start) = remaining.find('&') {
        output.push_str(&remaining[..start]);
        let after = &remaining[start + 1..];
        let Some(end) = after.find('-') else {
            return value.to_owned();
        };
        let encoded = &after[..end];
        if encoded.is_empty() {
            output.push('&');
        } else {
            let mut base64 = encoded.replace(',', "/");
            let padding = (4 - base64.len() % 4) % 4;
            base64.extend(std::iter::repeat_n('=', padding));
            let Ok(bytes) = STANDARD.decode(base64) else {
                return value.to_owned();
            };
            if bytes.len() % 2 != 0 {
                return value.to_owned();
            }
            let code_units = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            let Ok(decoded) = String::from_utf16(&code_units) else {
                return value.to_owned();
            };
            output.push_str(&decoded);
        }
        remaining = &after[end + 1..];
    }
    output.push_str(remaining);
    output
}

fn folder_matches(scope: &FolderScope, mailbox: &str) -> bool {
    // A role-qualified locator is internal catalogue identity. It is not a
    // user-visible hierarchy delimiter. Treating it as one would make an
    // internal locator public and make `in:Sent/*` match an unrelated remote
    // alias.
    let special = special_mailbox_parts(mailbox).or_else(|| {
        // Earlier catalogues stored resolved Spam and Trash aliases as
        // `Family::remote`. Those rows coexist with opaque locator rows while
        // the bounded migration advances. Retain their visible alias behavior
        // for search only; new provider rows use an opaque identity, and the
        // other families deliberately keep literal `Sent::Foo` semantics.
        legacy_spam_or_trash_mailbox_parts(mailbox)
            .map(|(family, remote)| (family, remote.to_owned()))
    });
    let generic = generic_mailbox_parts(mailbox);
    match scope {
        FolderScope::All => true,
        FolderScope::Exact(folder) => {
            let folder = normalize_search_text(folder);
            match special {
                Some((family, remote)) => {
                    // `::` appears only in the opaque storage spelling of a
                    // role locator. It is not public for special-use folders,
                    // but it remains valid literal text in an ordinary IMAP
                    // mailbox such as `Sent::Foo`.
                    if folder.contains("::") {
                        return false;
                    }
                    folder == normalize_search_text(family)
                        || folder == normalize_search_text(&remote)
                }
                None => generic.map_or_else(
                    || folder == normalize_search_text(mailbox),
                    |display| folder == normalize_search_text(&display),
                ),
            }
        }
        FolderScope::Descendants(folder) => {
            let folder = normalize_search_text(folder);
            // A nested wildcard operates on the visible provider path. For a
            // resolved special mailbox this is its remote alias, not `Family`.
            // Thus `in:Sent/*` does not treat `Sent::Sent Messages` as a child,
            // while `in:"[Gmail]/*"` can find `Sent::[Gmail]/Sent Mail`.
            let visible_path = special
                .as_ref()
                .map(|(_, remote)| remote.as_str())
                .or(generic.as_deref())
                .unwrap_or(mailbox);
            if special.is_some() && folder.contains("::") {
                return false;
            }
            path_is_descendant(visible_path, &folder)
        }
    }
}

fn special_mailbox_parts(mailbox: &str) -> Option<(&str, String)> {
    let (family, remote) = mailbox.split_once("::")?;
    let encoded = remote.strip_prefix(SPECIAL_MAILBOX_LOCATOR_PREFIX)?;
    let remote = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
    let remote = display_imap_mailbox_name(&remote);
    (!remote.is_empty() && is_special_family(family)).then_some((family, remote))
}

/// Transitional compatibility for old role-qualified mailbox keys. A legacy
/// `Spam::Bulk` or `Trash::Deleted` row has no unambiguous metadata after it
/// was persisted, so it retains the established system-folder behavior until
/// the storage migration rewrites it. Do not extend this to Sent et al.: a
/// legal ordinary provider mailbox such as `Sent::Foo` must remain literal.
fn legacy_spam_or_trash_mailbox_parts(mailbox: &str) -> Option<(&str, &str)> {
    let (family, remote) = mailbox.split_once("::")?;
    (!remote.is_empty()
        && (family.eq_ignore_ascii_case("Spam") || family.eq_ignore_ascii_case("Trash")))
    .then_some((family, remote))
}

fn generic_mailbox_parts(mailbox: &str) -> Option<String> {
    let payload = mailbox.strip_prefix(GENERIC_MAILBOX_LOCATOR_PREFIX)?;
    let (_, display) = payload.split_once(':')?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(display).ok()?).ok()
}

fn is_special_family(family: &str) -> bool {
    ["Sent", "Drafts", "Archive", "Spam", "Trash"]
        .iter()
        .any(|candidate| family.eq_ignore_ascii_case(candidate))
}

fn path_is_descendant(path: &str, folder: &str) -> bool {
    let path = normalize_search_text(path);
    path == folder
        || path
            .strip_prefix(folder)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn date_matches(
    comparison: DateComparison,
    query: &SearchDate,
    received_on: NaiveDate,
    today: NaiveDate,
) -> bool {
    let Some(target) = resolve_date(query, today) else {
        return false;
    };
    match comparison {
        DateComparison::On => received_on == target,
        DateComparison::Before => received_on < target,
        DateComparison::After => received_on > target,
    }
}

fn resolve_date(query: &SearchDate, today: NaiveDate) -> Option<NaiveDate> {
    match query {
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

fn state_matches(state: MessageState, message: &SearchableMessage<'_>) -> bool {
    match state {
        MessageState::Read => message.is_read,
        MessageState::Unread => !message.is_read,
        MessageState::Flagged => message.is_flagged,
        MessageState::Unflagged => !message.is_flagged,
        MessageState::Replied => message.is_replied,
        MessageState::Unreplied => !message.is_replied,
        MessageState::Draft => message.is_draft,
        MessageState::Undraft => !message.is_draft,
    }
}

fn file_type_matches(file_type: FileType, attachment: &SearchableAttachment<'_>) -> bool {
    let mime = attachment.mime_type.unwrap_or("").to_ascii_lowercase();
    let filename = attachment.filename.unwrap_or("").to_ascii_lowercase();
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or("");
    match file_type {
        FileType::Pdf => mime == "application/pdf" || extension == "pdf",
        FileType::Document => {
            (mime.starts_with("text/") && mime != "text/calendar")
                || mime.contains("word")
                || matches!(extension, "doc" | "docx" | "odt" | "rtf" | "txt")
        }
        FileType::Spreadsheet => {
            mime.contains("spreadsheet")
                || mime.contains("excel")
                || matches!(extension, "xls" | "xlsx" | "ods" | "csv")
        }
        FileType::Presentation => {
            mime.contains("presentation")
                || mime.contains("powerpoint")
                || matches!(extension, "ppt" | "pptx" | "odp")
        }
        FileType::Image => {
            mime.starts_with("image/")
                || matches!(
                    extension,
                    "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "svg"
                )
        }
        FileType::Audio => {
            mime.starts_with("audio/")
                || matches!(extension, "mp3" | "wav" | "m4a" | "ogg" | "flac")
        }
        FileType::Video => {
            mime.starts_with("video/")
                || matches!(extension, "mp4" | "mov" | "mkv" | "webm" | "avi")
        }
        FileType::Archive => {
            mime.contains("zip")
                || mime.contains("compressed")
                || matches!(
                    extension,
                    "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar"
                )
        }
        FileType::Calendar => mime == "text/calendar" || matches!(extension, "ics" | "ical"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::parse_search_query;

    const ATTACHMENTS: &[SearchableAttachment<'static>] = &[
        SearchableAttachment {
            filename: Some("Quarterly Invoice.PDF"),
            mime_type: Some("application/pdf"),
        },
        SearchableAttachment {
            filename: Some("invitation.ics"),
            mime_type: Some("text/calendar"),
        },
    ];

    fn message() -> SearchableMessage<'static> {
        SearchableMessage {
            from: "Åsa Example <asa@example.test>",
            to: "billing@example.test",
            cc: "team@example.test",
            bcc: "audit@example.test",
            subject: "Quarterly, report",
            body: "The invoice is attached. Please review it today from the oﬀice.",
            mailboxes: &[],
            mailbox: "Projects/2026",
            received_on: NaiveDate::from_ymd_opt(2026, 9, 6).unwrap(),
            attachments: ATTACHMENTS,
            is_read: true,
            is_flagged: true,
            is_replied: true,
            is_draft: false,
        }
    }

    fn matches(query: &str) -> bool {
        evaluate_search(
            &parse_search_query(query).unwrap(),
            &message(),
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        )
    }

    #[test]
    fn normalizes_diacritics_case_and_punctuation_without_stemming() {
        assert!(matches("ASA"));
        assert_eq!(normalize_search_text("Straße Σς"), "strasse σσ");
        assert_eq!(normalize_search_text("oﬀice ſtaff"), "office staff");
        assert_eq!(normalize_search_text("ϐϑϕϖϰϱϵ"), "βθφπκρε");
        assert!(matches("body:office"));
        assert!(matches("subject:quarterly"));
        assert!(!matches("reporting"));
        assert!(matches("invoic*"));
        assert!(!matches("voice*"));
    }

    #[test]
    fn decodes_modified_utf7_only_for_visible_mailbox_matching() {
        // RFC 3501's published modified UTF-7 examples. Their original wire
        // names stay encoded in the locator, while search sees Unicode.
        assert_eq!(display_imap_mailbox_name("&ZeVnLIqe-"), "日本語");
        assert_eq!(display_imap_mailbox_name("&U,BTFw-"), "台北");
        assert_eq!(display_imap_mailbox_name("Finance&-Legal"), "Finance&Legal");
        assert_eq!(
            display_imap_mailbox_name("bad&not-base64"),
            "bad&not-base64",
            "invalid modified UTF-7 is retained exactly rather than guessed"
        );

        let storage = generic_mailbox_storage_identity("&ZeVnLIqe-", "日本語");
        let mut mail = message();
        mail.mailbox = &storage;
        assert!(evaluate_search(
            &parse_search_query("in:日本語").unwrap(),
            &mail,
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        ));
    }

    #[test]
    fn quoted_text_requires_adjacent_normalized_tokens() {
        assert!(matches("subject:\"quarterly report\""));
        assert!(matches("subject:'quarterly, report'"));
        assert!(!matches("subject:\"report quarterly\""));
        assert!(!matches("subject:\"quarterly invoice\""));
        // A quoted asterisk is punctuation, not an implicit prefix operator.
        assert!(!matches("subject:'quarter*'"));
    }

    #[test]
    fn regex_looking_punctuation_is_token_separation_not_a_search_mode() {
        assert!(matches("subject:/quarterly/"));
        assert!(matches("subject:^quarterly$"));
        assert!(matches("subject:[quarterly]"));
        assert!(matches("quarter*"));
        assert!(!matches(".*"));
    }

    #[test]
    fn evaluates_fields_folders_dates_attachments_and_states() {
        assert!(matches("from:asa with:billing tonotcc:billing"));
        assert!(!matches("tonotcc:team"));
        assert!(matches("to:team to:audit"));
        assert!(matches("in:Projects/* date:2026-09-06 before:1d after:3d"));
        assert!(matches(
            "has:attachment filename:invoice* filetype:pdf filetype:calendar"
        ));
        assert!(matches("is:seen is:pinned is:answered is:undraft"));
        assert!(!matches("has:noattachment OR is:unread"));
    }

    #[test]
    fn folder_predicates_match_any_logical_membership_or_the_legacy_mailbox() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();
        let memberships = ["Clients", "Invoices"];
        let mut mail = message();
        mail.mailbox = "Legacy/Archive";
        mail.mailboxes = &memberships;

        assert!(evaluate_search(
            &parse_search_query("in:Clients in:Invoices").unwrap(),
            &mail,
            today,
        ));
        assert!(!evaluate_search(
            &parse_search_query("in:Projects").unwrap(),
            &mail,
            today,
        ));

        mail.mailboxes = &[];
        assert!(evaluate_search(
            &parse_search_query("in:Legacy/*").unwrap(),
            &mail,
            today,
        ));
    }

    #[test]
    fn every_file_type_matches_its_canonical_attachment_fixture() {
        let fixtures = [
            ("pdf", "invoice.pdf", "application/pdf"),
            (
                "document",
                "meeting-notes.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "spreadsheet",
                "budget.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "presentation",
                "roadmap.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("image", "logo.png", "image/png"),
            ("audio", "briefing.mp3", "audio/mpeg"),
            ("video", "demo.mp4", "video/mp4"),
            ("archive", "source.zip", "application/zip"),
            ("calendar", "invite.ics", "text/calendar"),
        ];
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        for (file_type, filename, mime_type) in fixtures {
            let attachments = [SearchableAttachment {
                filename: Some(filename),
                mime_type: Some(mime_type),
            }];
            let mut mail = message();
            mail.attachments = &attachments;

            assert!(
                evaluate_search(
                    &parse_search_query(&format!("filetype:{file_type}")).unwrap(),
                    &mail,
                    today,
                ),
                "{file_type} must match its canonical fixture"
            );
            assert!(
                !evaluate_search(
                    &parse_search_query("filetype:calendar").unwrap(),
                    &mail,
                    today,
                ) || file_type == "calendar",
                "a non-calendar fixture must not match calendar"
            );
        }
    }

    #[test]
    fn every_state_and_alias_has_positive_and_negative_truth_cases() {
        let cases = [
            ("is:read", "is:seen", "is:unread", true, false, false, false),
            (
                "is:unread",
                "is:unseen",
                "is:read",
                false,
                false,
                false,
                false,
            ),
            (
                "is:flagged",
                "is:pinned",
                "is:unflagged",
                false,
                true,
                false,
                false,
            ),
            (
                "is:unflagged",
                "is:unpinned",
                "is:flagged",
                false,
                false,
                false,
                false,
            ),
            (
                "is:replied",
                "is:answered",
                "is:unreplied",
                false,
                false,
                true,
                false,
            ),
            (
                "is:unreplied",
                "is:unanswered",
                "is:replied",
                false,
                false,
                false,
                false,
            ),
            (
                "is:draft",
                "state:draft",
                "is:undraft",
                false,
                false,
                false,
                true,
            ),
            (
                "is:undraft",
                "state:undraft",
                "is:draft",
                false,
                false,
                false,
                false,
            ),
        ];
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        for (positive, alias, negative, read, flagged, replied, draft) in cases {
            let mut mail = message();
            mail.is_read = read;
            mail.is_flagged = flagged;
            mail.is_replied = replied;
            mail.is_draft = draft;
            for query in [positive, alias] {
                assert!(
                    evaluate_search(&parse_search_query(query).unwrap(), &mail, today),
                    "{query} must match its enabled state"
                );
            }
            assert!(
                !evaluate_search(&parse_search_query(negative).unwrap(), &mail, today),
                "{negative} must not match its enabled inverse state"
            );
        }
    }

    #[test]
    fn every_relative_date_unit_uses_its_resolved_calendar_day() {
        let today = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let cases = [
            ("1d", NaiveDate::from_ymd_opt(2024, 3, 30).unwrap()),
            ("1w", NaiveDate::from_ymd_opt(2024, 3, 24).unwrap()),
            ("1m", NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()),
            ("1y", NaiveDate::from_ymd_opt(2023, 3, 31).unwrap()),
        ];

        for (relative, target) in cases {
            let expression = parse_search_query(&format!("date:{relative}")).unwrap();
            let mut mail = message();
            mail.received_on = target;
            assert!(
                evaluate_search(&expression, &mail, today),
                "{relative} must match its calendar target"
            );
            mail.received_on = target.succ_opt().unwrap();
            assert!(
                !evaluate_search(&expression, &mail, today),
                "{relative} must not match an adjacent day"
            );
        }
    }

    #[test]
    fn fastmail_to_checks_every_recipient_but_tonotcc_checks_to_only() {
        let mut copied = message();
        copied.to = "same@example.test";
        copied.cc = "same@example.test";
        copied.bcc = "blind@example.test";
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        assert!(evaluate_search(
            &parse_search_query("to:same").unwrap(),
            &copied,
            today,
        ));
        assert!(evaluate_search(
            &parse_search_query("to:blind").unwrap(),
            &copied,
            today,
        ));
        // A Cc duplicate must not negate a real To match.
        assert!(evaluate_search(
            &parse_search_query("tonotcc:same").unwrap(),
            &copied,
            today,
        ));
        assert!(!evaluate_search(
            &parse_search_query("tonotcc:blind").unwrap(),
            &copied,
            today,
        ));
    }

    #[test]
    fn relative_calendar_dates_use_a_stable_reference_day() {
        let expression = parse_search_query("date:1m").unwrap();
        let mut january = message();
        january.received_on = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        assert!(evaluate_search(
            &expression,
            &january,
            NaiveDate::from_ymd_opt(2024, 3, 31).unwrap()
        ));
    }

    #[test]
    fn resolved_special_mailbox_storage_ids_match_visible_family_and_remote_alias() {
        let mut sent = message();
        let sent_storage = special_mailbox_storage_identity("Sent", "Sent Messages");
        sent.mailbox = &sent_storage;
        assert!(evaluate_search(
            &parse_search_query("in:Sent").unwrap(),
            &sent,
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        ));
        assert!(evaluate_search(
            &parse_search_query("in:\"sent messages\"").unwrap(),
            &sent,
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        ));
        assert!(!evaluate_search(
            &parse_search_query("in:Sent::Sent").unwrap(),
            &sent,
            NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
        ));

        // These opaque storage shapes are produced by special-use resolution
        // for provider folders whose visible names differ from Dakia's roles.
        for (family, remote) in [
            ("Drafts", "[Gmail]/Drafts"),
            ("Archive", "[Gmail]/All Mail"),
            ("Spam", "Bulk"),
            ("Trash", "Deleted Items"),
        ] {
            let mut special = message();
            let storage = special_mailbox_storage_identity(family, remote);
            special.mailbox = &storage;
            assert!(evaluate_search(
                &parse_search_query(&format!("in:{family}")).unwrap(),
                &special,
                NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
            ));
            assert!(evaluate_search(
                &parse_search_query(&format!("in:\"{remote}\"")).unwrap(),
                &special,
                NaiveDate::from_ymd_opt(2026, 9, 8).unwrap(),
            ));
        }
    }

    #[test]
    fn special_locator_never_reclassifies_a_literal_provider_mailbox_name() {
        let mut special = message();
        let locator = special_mailbox_storage_identity("Sent", "Foo");
        special.mailbox = &locator;
        let mut ordinary = message();
        let ordinary_locator = generic_mailbox_storage_identity("Sent::Foo", "Sent::Foo");
        ordinary.mailbox = &ordinary_locator;
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        assert!(evaluate_search(
            &parse_search_query("in:Sent").unwrap(),
            &special,
            today,
        ));
        assert!(evaluate_search(
            &parse_search_query("in:Foo").unwrap(),
            &special,
            today,
        ));
        assert!(!evaluate_search(
            &parse_search_query("in:Sent").unwrap(),
            &ordinary,
            today,
        ));
        assert!(!evaluate_search(
            &parse_search_query("in:Foo").unwrap(),
            &ordinary,
            today,
        ));
        assert!(evaluate_search(
            &parse_search_query("in:\"Sent::Foo\"").unwrap(),
            &ordinary,
            today,
        ));
    }

    #[test]
    fn legacy_spam_and_trash_aliases_remain_visible_while_migration_is_bounded() {
        let mut spam = message();
        spam.mailbox = "Spam::Bülk/Child";
        let mut trash = message();
        trash.mailbox = "Trash::Bîn/Child";
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        assert!(evaluate_search(
            &parse_search_query("in:Spam").unwrap(),
            &spam,
            today,
        ));
        assert!(evaluate_search(
            &parse_search_query("in:bulk/*").unwrap(),
            &spam,
            today,
        ));
        assert!(evaluate_search(
            &parse_search_query("in:BIN/*").unwrap(),
            &trash,
            today,
        ));
    }

    #[test]
    fn special_folder_aliases_do_not_break_visible_nested_wildcards() {
        let mut sent = message();
        let sent_storage = special_mailbox_storage_identity("Sent", "[Gmail]/Sent Mail");
        sent.mailbox = &sent_storage;
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        // The internal `::` identity is never a visible child relationship.
        assert!(!evaluate_search(
            &parse_search_query("in:Sent/*").unwrap(),
            &sent,
            today,
        ));
        // The actual visible remote hierarchy still observes the documented
        // slash wildcard behavior.
        assert!(evaluate_search(
            &parse_search_query("in:\"[Gmail]/*\"").unwrap(),
            &sent,
            today,
        ));

        let mut regular = message();
        regular.mailbox = "Projects/2026/Invoices";
        assert!(evaluate_search(
            &parse_search_query("in:Projects/*").unwrap(),
            &regular,
            today,
        ));
    }
}
