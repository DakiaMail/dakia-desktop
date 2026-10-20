use crate::{
    account::Account,
    provider,
    search::{
        parse_search_query, AttachmentPredicate, FolderScope, SearchExpression, SearchNode,
        SearchTerm,
    },
    search_eval::{
        display_imap_mailbox_name, evaluate_search, generic_mailbox_storage_identity,
        normalize_search_text, special_mailbox_storage_identity, SearchableAttachment,
        SearchableMessage,
    },
    search_session::SearchMatchEvidence,
    search_sql::{compile_sql_candidate, SqlSearchBind, SqlSearchCandidate},
    AccountAuth, AccountId,
};
use anyhow::{anyhow, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use chrono::{DateTime, Utc};
use mail_parser::{Address as ParsedAddress, HeaderName, HeaderValue, MessageParser};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM},
    rand::{SecureRandom, SystemRandom},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    FromRow, Sqlite, SqlitePool, Transaction,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, FileTimes, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const VAULT_KEY_FILE: &str = "vault.key";
const VAULT_KEY_LEN: usize = 32;
const VAULT_NONCE_LEN: usize = 12;
/// Foreground-opened, non-starred mail is useful offline, but it must never
/// grow into a second unbounded mail store. Starred mail has its own durable,
/// authoritative cache and is intentionally not counted here.
const MESSAGE_CONTENT_CACHE_MAX_BYTES: i64 = 512 * 1024 * 1024;
const MESSAGE_CONTENT_CACHE_RECENT_WINDOW_DAYS: i64 = 30;
/// Complete text parts fetched for provider search are useful to subsequent
/// local result pages, but are not reader-ready content. Keep their separate
/// text-only cache bounded so a broad remote search cannot grow without bound
/// or displace the reader cache's HTML and attachment metadata.
const MESSAGE_SEARCH_BODY_TEXT_CACHE_MAX_BYTES: i64 = 256 * 1024 * 1024;
const SEARCH_CATALOGUE_V2_MIGRATION_BATCH_SIZE: i64 = 500;
/// Older profiles need a small, durable upgrade for autocomplete's normalized
/// search data and provider-safe Sent source markers. Keep this deliberately
/// separate from the search catalogue migration: opening a profile must not
/// read its complete contacted-people history into memory.
const CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE: i64 = 500;
const CONTACTED_PEOPLE_NORMALIZED_MIGRATION_CURSOR_KEY: &str =
    "contacted_people_normalized_migration_cursor";
const CONTACTED_PEOPLE_NORMALIZED_MIGRATION_COMPLETE_KEY: &str =
    "contacted_people_normalized_migration_complete";
const CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY: &str =
    "contacted_people_source_migration_cursor";
const CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY: &str =
    "contacted_people_source_migration_complete";
/// Existing profiles stored all-account totals on `contacted_people`. Version
/// one rebuilds those denormalized fields from enabled accounts once, so the
/// fast suggestion path can use them without grouping every account stat on
/// each keystroke.
const CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY: &str =
    "contacted_people_enabled_aggregates_version";
const CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY: &str =
    "contacted_people_enabled_aggregates_cursor";
/// Marks the one-way conversion from historical human-readable mailbox
/// namespaces to provider-safe opaque namespaces.  This is intentionally
/// separate from the search-index marker: it changes durable locator keys,
/// not the indexed search data itself.
const OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE: i64 = 500;
/// Attachment presentation and opaque IDs change when the selected HTML/MIME
/// branch changes. Version 3 invalidates IDs written by the full-message
/// parser, whose downloadable-only numbering can otherwise select a different
/// part when interpreted by the sectioned MIME planner, and cached attachment
/// names written before selective BODYSTRUCTURE decoding handled RFC 2047
/// parameters.
const ATTACHMENT_PRESENTATION_CACHE_VERSION: &str = "3";
const ATTACHMENT_PRESENTATION_VERSION: i64 = 3;
/// Classification policy is versioned independently from the bundled model.
/// A policy change must re-run model-owned classifications while preserving
/// categories the user chose explicitly.
const CLASSIFICATION_POLICY_VERSION: &str = "2";
/// Prevent two concurrently running app/CLI versions from repeatedly claiming
/// different classifier revisions and requeueing each other's results. A
/// normal batch is far shorter than this; an inactive/crashed process yields
/// automatically without a durable lock.
const CLASSIFICATION_REVISION_ACTIVITY_SECONDS: i64 = 90;
/// Foreground readers wait at most 60 seconds for another body fetch. Keep a
/// short grace period beyond that before a crashed process's lease may be
/// replaced, while preserving fresh claims across concurrently open processes.
const MESSAGE_CONTENT_FETCH_LEASE_SECONDS: i64 = 90;
const MAILBOX_SNAPSHOT_REPLACEMENT_PUBLISH_BATCH_SIZE: i64 = 100;
/// All stores opened by this process share a fetch-claim owner. This lets a
/// second connection respect work already in flight, while a new process can
/// discard claims left by the previous process during migration.
static MESSAGE_CONTENT_FETCH_OWNER: OnceLock<String> = OnceLock::new();

fn message_content_fetch_owner() -> &'static str {
    MESSAGE_CONTENT_FETCH_OWNER
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    vault_key: Arc<[u8; VAULT_KEY_LEN]>,
}

#[derive(Clone, Copy)]
enum SnapshotFinalizeWatermark {
    StagedMaximum,
    Explicit(Option<u32>),
}

#[derive(Clone, Copy)]
enum SnapshotReplacementPublication<'a> {
    None,
    InMemory(&'a [MailSummary]),
    Staged,
}

/// The remote identity captured when a full mailbox inventory begins.
///
/// Grouping these values keeps the start and resume contract explicit: all
/// fields must describe the same selected mailbox response.
#[derive(Clone, Copy)]
pub struct MailboxSnapshotIdentity<'a> {
    pub remote_name: &'a str,
    pub uid_validity: u32,
    pub initial_exists: u32,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
}

impl<'a> MailboxSnapshotIdentity<'a> {
    pub const fn new(
        remote_name: &'a str,
        uid_validity: u32,
        initial_exists: u32,
        uid_next: Option<u32>,
        highest_modseq: Option<u64>,
    ) -> Self {
        Self {
            remote_name,
            uid_validity,
            initial_exists,
            uid_next,
            highest_modseq,
        }
    }
}

/// One complete CONDSTORE `CHANGEDSINCE` result and the mailbox state that
/// made it valid.
pub struct MailboxChangedSinceFlags<'a> {
    pub identity: MailboxSnapshotIdentity<'a>,
    pub remote_total: usize,
    pub flags: &'a [(u32, bool, bool)],
}

type MailboxSnapshotGenerationState = (String, i64, i64, Option<i64>, Option<String>);
type ContactedPeopleSourceMigrationRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);
type SelectableMailboxIdentityRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
);
type SearchAttachment = (Option<String>, Option<String>);
type LegacyContactedPeopleMailboxCandidate =
    (String, String, Option<String>, Option<String>, Option<i64>);

/// Values that are atomically published after cataloguing a mailbox.
///
/// Keep the fields together so the generation check cannot accidentally be
/// paired with data from a different selected mailbox response.
struct MailboxCatalogStateWrite<'a> {
    mailbox: &'a str,
    remote_name: &'a str,
    uid_validity: u32,
    remote_total: usize,
    historical_complete: bool,
}

/// One outgoing recipient that may be learned after SMTP accepts a message.
///
/// The address is authoritative. `formatted_address` is optional because the
/// SMTP envelope commonly has no display name; when absent, the store creates
/// a safe RFC-style display form from `display_name` and `address`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactedPersonRecipient {
    pub address: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub formatted_address: Option<String>,
}

impl ContactedPersonRecipient {
    pub fn address_only(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            display_name: None,
            formatted_address: None,
        }
    }
}

/// A local autocomplete result. Counts are intentionally included so callers
/// can retain an already-ranked list while the compose account changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, FromRow)]
pub struct ContactedPersonSuggestion {
    pub address: String,
    pub display_name: Option<String>,
    pub formatted_address: String,
    pub first_contacted_at: DateTime<Utc>,
    pub last_contacted_at: DateTime<Utc>,
    pub send_count: i64,
    pub account_send_count: i64,
    pub account_last_contacted_at: Option<DateTime<Utc>>,
    /// The preferred account contribution that supplied the affinity fields.
    /// Global fallback suggestions have no account-specific backing row.
    pub account_id: Option<String>,
    /// Suggestions normally exclude hidden people, but exposing the state
    /// keeps the public contract explicit and safe for future administrative
    /// views that may include them.
    pub hidden: bool,
}

/// Result of one bounded historical Sent-mail scan. A completed scan can be
/// called again safely: marker rows make newly catalogued older Sent messages
/// discoverable without counting previously seen messages a second time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactedPeopleBackfillProgress {
    pub processed_messages: usize,
    /// Number of recipients whose suggestion statistics changed in this
    /// batch. Suppressed, empty, duplicate, and SMTP-correlated rows leave
    /// this at zero so callers need not refresh autocomplete UI state.
    pub changed_people: usize,
    pub complete: bool,
}

/// Progress for the one-time, restart-safe contacted-people schema upgrades.
/// `changed_people` deliberately remains zero: these upgrades only add search
/// keys and idempotency markers, never recipient statistics or suggestions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactedPeopleMigrationProgress {
    pub normalized_people: usize,
    pub source_markers: usize,
    pub changed_people: usize,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy)]
struct ContactedPeopleMigrationBatch {
    processed: usize,
    complete: bool,
}

/// Cancellation-safe ownership of one provider body fetch. Dropping the
/// owning future schedules claim release so later readers do not wait for a
/// process restart.
pub struct MessageContentFetchClaim {
    store: Store,
    message_id: String,
    owner: String,
    renewal: tokio::task::JoinHandle<()>,
    released: bool,
}

/// Outcome of attempting to own one provider body fetch.
///
/// A reader needs to distinguish another live owner from a message that has
/// already been moved or removed. Treating both as "not acquired" makes a
/// stale UI row wait for the full fetch timeout.
pub enum MessageContentFetchAcquire {
    Claimed(MessageContentFetchClaim),
    Busy,
    Missing,
}

impl MessageContentFetchClaim {
    pub async fn release(mut self) -> Result<()> {
        self.renewal.abort();
        self.store
            .release_message_content_fetch_for_owner(&self.message_id, &self.owner)
            .await?;
        self.released = true;
        Ok(())
    }
}

impl Drop for MessageContentFetchClaim {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.renewal.abort();
        let store = self.store.clone();
        let message_id = self.message_id.clone();
        let owner = self.owner.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = store
                    .release_message_content_fetch_for_owner(&message_id, &owner)
                    .await;
            });
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRebuildJob {
    pub account_id: AccountId,
    pub phase: String,
    pub completed: usize,
    pub total: Option<usize>,
    pub reset_before_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, FromRow)]
pub struct MailSummary {
    pub id: String,
    pub account_id: String,
    pub mailbox: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub reference_ids: Option<String>,
    pub thread_id: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_address: String,
    pub to_addresses: String,
    /// Decoded RFC 5322 Cc header, retained exactly as supplied by the provider.
    /// Empty means the header was absent; recipients are never inferred.
    pub cc_addresses: String,
    /// Decoded RFC 5322 Bcc header. Providers normally omit this for received
    /// mail, so an empty value is intentionally distinct from inferred data.
    pub bcc_addresses: String,
    /// Decoded RFC 5322 Reply-To header, retained without falling back to From.
    pub reply_to_addresses: String,
    pub received_at: DateTime<Utc>,
    pub snippet: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub content_state: String,
    pub unsubscribe_kind: Option<String>,
    #[serde(skip_serializing)]
    pub unsubscribe_url: Option<String>,
    pub is_read: bool,
    pub is_flagged: bool,
    /// Provider `\\Answered` state. False only means the provider did not
    /// report the flag or explicitly cleared it.
    #[serde(default)]
    pub is_answered: bool,
    /// Provider `\\Draft` state. This is not inferred from mailbox names.
    #[serde(default)]
    pub is_draft: bool,
    pub has_attachments: bool,
    pub category: Option<String>,
    pub classification_confidence: Option<f64>,
    pub classification_source: Option<String>,
    /// Non-content RFC header signals retained for local categorization.
    pub classification_signals: String,
    #[serde(skip)]
    #[sqlx(skip)]
    pub attachments: Vec<AttachmentData>,
}

/// Local flags observed before a remote catalogue fetch. They form the
/// compare-and-swap precondition when the delayed fetch is published.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ExpectedMessageFlags {
    pub account_id: String,
    pub mailbox: String,
    pub uid: i64,
    pub is_read: bool,
    pub is_flagged: bool,
}

#[derive(Debug, Clone)]
pub struct MailboxSyncState {
    pub initialized: bool,
    pub highest_uid: Option<u32>,
    pub uid_validity: Option<u64>,
    /// The selected mailbox is a new UID namespace and needs a replacement
    /// catalogue, not an incremental UID sync.
    pub uid_validity_changed: bool,
}

/// Provider-discovered mailbox identity. `id` is a local opaque identifier;
/// callers must not derive it from the provider path or hierarchy delimiter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SelectableMailbox {
    pub id: String,
    pub account_id: String,
    /// Provider path used in IMAP commands.
    pub remote_path: String,
    /// Stable local catalogue path. This initially matches `remote_path`, but
    /// is kept separate so provider delimiters never leak into local scope.
    pub local_path: String,
    pub hierarchy_delimiter: Option<String>,
    pub parent_id: Option<String>,
    pub parent_path: Option<String>,
    pub special_use: Option<String>,
    pub selectable: bool,
    pub uid_validity: Option<i64>,
    /// `unknown`, `partial`, or `complete`; providers can update it without
    /// changing the mailbox identity row.
    pub catalogue_coverage: String,
}

/// Input for an idempotent provider mailbox discovery update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SelectableMailboxDraft {
    pub remote_path: String,
    #[serde(default)]
    pub local_path: Option<String>,
    #[serde(default)]
    pub hierarchy_delimiter: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub parent_path: Option<String>,
    #[serde(default)]
    pub special_use: Option<String>,
    #[serde(default = "selectable_by_default")]
    pub selectable: bool,
    #[serde(default)]
    pub uid_validity: Option<i64>,
    #[serde(default = "unknown_catalogue_coverage")]
    pub catalogue_coverage: String,
}

fn selectable_by_default() -> bool {
    true
}

fn unknown_catalogue_coverage() -> String {
    "unknown".into()
}

/// A logical local message locator and one of its provider mailbox members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct MessageMailboxMembership {
    pub message_id: String,
    pub mailbox_id: String,
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailConversation {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub messages: Vec<MailSummary>,
    /// Every concrete mailbox/UID locator, including logical Message-ID
    /// copies that are collapsed in `messages` for display.
    pub source_messages: Vec<MailSummary>,
    pub latest: MailSummary,
    pub message_count: usize,
    pub unread: bool,
    pub has_attachments: bool,
    pub participants: Vec<String>,
}

/// Position immediately after a message in the descending mailbox ordering.
///
/// The pair is deliberately part of the public query contract: timestamps are
/// not unique when a provider imports or restores a batch of mail.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct MailCursor {
    pub received_at: DateTime<Utc>,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailConversationPage {
    pub conversations: Vec<MailConversation>,
    /// Additive exact-match evidence for search pages. The legacy
    /// `search_conversations` API still returns conversations alone.
    #[serde(default)]
    pub match_evidence: BTreeMap<String, SearchMatchEvidence>,
    pub next_cursor: Option<MailCursor>,
    /// Internal keyset progress for V2 local search. It is deliberately not
    /// serialized: the desktop continuation is the only public carrier.
    #[serde(skip)]
    pub candidate_cursor: Option<MailCursor>,
    #[serde(skip)]
    pub candidate_exhausted: bool,
}

/// Durable progress for the yielding v2 search catalogue backfill. The
/// header stage is the broad corpus limiter, so its values make coverage
/// state visible without exposing SQLite row IDs to desktop callers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchCatalogueV2BackfillProgress {
    pub indexed_messages: i64,
    pub total_messages: i64,
    pub complete: bool,
}

/// Per-account local body-search coverage. A complete catalogue does not
/// imply every message body is locally searchable: headers-only rows remain
/// eligible for provider search until one authoritative body cache is filled.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalBodyIndexCoverage {
    pub account_id: String,
    pub catalogue_messages: i64,
    pub searchable_bodies: i64,
}

struct SearchConversationMatches {
    representatives: Vec<MailSummary>,
    matched_by_thread: HashMap<(String, String), Vec<MailSummary>>,
    candidate_cursor: Option<MailCursor>,
    exhausted: bool,
}

/// Keep the work performed by a local search turn bounded even when the SQL
/// compiler deliberately broadens a predicate (NOT, incomplete catalogue,
/// and attachment predicates).  This is a candidate budget, not a result
/// limit: canonical Rust evaluation still decides every returned match.
const SEARCH_CANDIDATE_SCAN_CHUNK: usize = 256;
/// A draft preview or one V2 local page may inspect at most this many broad
/// SQL candidates.  A no-match NOT expression must yield an incomplete page
/// and continuation, not monopolize the SQLite connection until every one of
/// 50,000 rows has been evaluated.
const SEARCH_CANDIDATE_SCAN_BUDGET: usize = SEARCH_CANDIDATE_SCAN_CHUNK;

struct SearchCandidateChunk {
    rows: Vec<MailSummary>,
    matches: Vec<bool>,
    exhausted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SmartInboxSectionPage {
    pub id: String,
    pub conversations: Vec<MailConversation>,
    pub next_cursor: Option<MailCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmartInboxPage {
    pub sections: Vec<SmartInboxSectionPage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SmartInboxQuery {
    pub account_ids: Vec<AccountId>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ThreadingHeaders {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub reference_ids: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct MailboxCatalogState {
    pub account_id: String,
    pub mailbox: String,
    pub remote_name: String,
    pub uid_validity: i64,
    pub remote_total: i64,
    pub historical_complete: bool,
    /// The provider's next UID at the last complete, identity-stable sync.
    pub uid_next: Option<i64>,
    /// Stored as decimal text because IMAP MODSEQ is an unsigned 64-bit value
    /// while SQLite INTEGER is signed.
    pub highest_modseq: Option<String>,
}

/// A durable, generation-scoped inventory of the UIDs and flags observed
/// while reconciling one IMAP mailbox. A generation remains non-authoritative
/// until [`Store::finalize_mailbox_snapshot`] succeeds.
///
/// The provider's message metadata is still published through the ordinary
/// catalogue path. Keeping the inventory separate means an interrupted page
/// fetch can never make an incomplete UID list look like an empty mailbox.
#[derive(Debug, Clone, FromRow)]
pub struct MailboxSnapshotGeneration {
    pub account_id: String,
    pub mailbox: String,
    pub generation: String,
    pub remote_name: String,
    pub uid_validity: i64,
    pub initial_exists: i64,
    pub uid_next: Option<i64>,
    pub highest_modseq: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A UID whose catalogue metadata could not be fetched or parsed. These rows
/// deliberately outlive the current IMAP session so a later high-water mark
/// cannot silently make the message permanently invisible.
#[derive(Debug, Clone, FromRow)]
pub struct MailboxSyncFailure {
    pub account_id: String,
    pub mailbox: String,
    pub uid: i64,
    pub stage: String,
    pub error: String,
    pub updated_at: DateTime<Utc>,
}

/// Display metadata for an attachment. Bytes stay in the local store and are
/// retrieved only by an opaque attachment id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, FromRow)]
pub struct Attachment {
    pub id: String,
    pub message_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub is_inline: bool,
    /// Whether this part is used by the selected rendered HTML, is available
    /// for download, or both. `Unknown` is only accepted while reading legacy
    /// serialized metadata and is invalidated before it reaches callers.
    #[serde(default)]
    pub presentation: AttachmentPresentation,
    pub is_potentially_unsafe: bool,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, sqlx::Type,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum AttachmentPresentation {
    #[default]
    Unknown,
    Embedded,
    Downloadable,
    Both,
}

impl AttachmentPresentation {
    pub fn is_downloadable(self) -> bool {
        matches!(self, Self::Downloadable | Self::Both)
    }

    fn is_current(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone)]
pub struct AttachmentData {
    pub attachment: Attachment,
    pub bytes: Vec<u8>,
}

/// Complete, display-safe content cached after an authoritative foreground
/// fetch. Attachment bytes are deliberately absent.
#[derive(Debug, Clone)]
pub struct CachedMessageContent {
    pub body_text: String,
    pub body_html: Option<String>,
    pub unsubscribe_kind: Option<String>,
    pub attachments: Vec<Attachment>,
}

/// One model decision tied to the exact catalogue evidence used for inference.
/// Applying it is compare-and-swap: a concurrent sync that changes any input
/// makes the result stale instead of letting it overwrite newer evidence.
#[derive(Debug, Clone)]
pub struct ModelClassificationUpdate {
    id: String,
    category: String,
    confidence: Option<f64>,
    expected_from_name: Option<String>,
    expected_from_address: String,
    expected_subject: String,
    expected_snippet: String,
    expected_body_text: String,
    expected_signals: String,
    expected_known_correspondence: bool,
    expected_owner: String,
    expected_model_revision: String,
    expected_policy_revision: &'static str,
}

impl ModelClassificationUpdate {
    pub fn from_message(
        message: &MailSummary,
        category: String,
        confidence: Option<f64>,
        known_correspondence: bool,
        owner: &str,
        model_revision: &str,
    ) -> Self {
        Self {
            id: message.id.clone(),
            category,
            confidence,
            expected_from_name: message.from_name.clone(),
            expected_from_address: message.from_address.clone(),
            expected_subject: message.subject.clone(),
            expected_snippet: message.snippet.clone(),
            expected_body_text: message.body_text.clone(),
            expected_signals: message.classification_signals.clone(),
            expected_known_correspondence: known_correspondence,
            expected_owner: owner.to_owned(),
            expected_model_revision: model_revision.to_owned(),
            expected_policy_revision: CLASSIFICATION_POLICY_VERSION,
        }
    }
}

/// Minimal mailbox metadata used to refresh classification signals without
/// downloading or changing an email's body or user-selected category.
#[derive(Debug, Clone, FromRow)]
pub struct MailSignalMetadata {
    pub id: String,
    pub uid: i64,
    pub classification_signals: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchQuery {
    pub text: String,
    pub account_ids: Vec<AccountId>,
    pub mailbox: Option<String>,
    pub from: Option<String>,
    pub unread_only: bool,
    /// Include only conversations whose messages in the requested mailbox
    /// scope have all been read.
    #[serde(default)]
    pub read_only: bool,
    pub flagged_only: bool,
    /// Exclude every conversation containing a flagged message. This is kept
    /// separate from `flagged_only` because Smart category views use it to
    /// suppress an entire thread, not just its representative row.
    #[serde(default)]
    pub unflagged_only: bool,
    pub category: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<MailCursor>,
}

/// A durable conversation locator for surfaces that can outlive a concrete
/// mailbox/UID move. Resolution deliberately remains account-scoped and
/// fail-closed when a logical RFC Message-ID appears in multiple threads.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTarget {
    pub account_id: AccountId,
    #[serde(default)]
    pub local_message_id: Option<String>,
    #[serde(default)]
    pub rfc_message_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub mailbox: Option<String>,
}

#[derive(FromRow)]
struct LegacyAccountRow {
    id: String,
    email: String,
    display_name: Option<String>,
    host: String,
    port: i64,
    tls: i64,
    username: String,
    provider_capabilities: String,
    created_at: String,
}

#[derive(FromRow)]
struct LegacyMailboxRow {
    account_id: String,
    mailbox: String,
    remote_name: String,
    uid_validity: Option<i64>,
}

#[derive(FromRow)]
struct LegacyMessageRow {
    id: String,
    account_id: String,
    mailbox: String,
    uid: i64,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    reference_ids: Option<String>,
    thread_id: Option<String>,
    subject: Option<String>,
    from_address: String,
    to_addresses: String,
    date: Option<String>,
    flags: String,
    snippet: Option<String>,
}

fn legacy_capability(capabilities: &serde_json::Value, name: &str) -> Option<String> {
    capabilities
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn legacy_received_at(value: Option<&str>) -> DateTime<Utc> {
    value
        .and_then(|value| {
            DateTime::parse_from_rfc2822(value)
                .or_else(|_| DateTime::parse_from_rfc3339(value))
                .ok()
        })
        .map(|value| value.with_timezone(&Utc))
        // A legacy row without a valid message date must not be assigned the
        // migration time. Epoch is explicit and sorts safely behind dated mail.
        .unwrap_or_else(|| DateTime::<Utc>::UNIX_EPOCH)
}

fn legacy_created_at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S %z"))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .map(|value| value.and_utc().fixed_offset())
        })
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let key_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let vault_key = Arc::new(load_or_create_vault_key(&key_dir.join(VAULT_KEY_FILE))?);
        let options = SqliteConnectOptions::from_str(path.to_string_lossy().as_ref())?
            .create_if_missing(true)
            .foreign_keys(true)
            // Real-time account watchers publish independently. WAL lets
            // readers continue during those short writes, while the busy
            // timeout makes concurrent writers wait instead of dropping a
            // mailbox cycle with SQLITE_BUSY.
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(10));
        // IMAP work is parallel, but catalogue reads followed by deferred
        // write transactions can otherwise race while upgrading their SQLite
        // locks. A single local connection queues those short DB sections and
        // prevents an account watcher from losing an arrival to SQLITE_BUSY.
        let connect_deadline = Instant::now() + Duration::from_secs(10);
        let pool = loop {
            match SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options.clone())
                .await
            {
                Ok(pool) => break pool,
                Err(error)
                    if is_sqlite_busy_message(&error.to_string())
                        && Instant::now() < connect_deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let store = Self { pool, vault_key };
        store.migrate_with_busy_retry().await?;
        Ok(store)
    }

    pub async fn in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        let store = Self {
            pool,
            vault_key: Arc::new(random_bytes()?),
        };
        store.migrate_with_busy_retry().await?;
        Ok(store)
    }

    async fn migrate_with_busy_retry(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.migrate().await {
                Ok(()) => return Ok(()),
                Err(error)
                    if (is_sqlite_busy(&error) || is_sqlite_migration_race(&error))
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn migrate(&self) -> Result<()> {
        let migrated_legacy_profile = self.prepare_legacy_desktop_profile().await?;
        let sync_state_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'mailbox_sync_state')",
        )
        .fetch_one(&self.pool)
        .await?;
        for statement in [
            "CREATE TABLE IF NOT EXISTS accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, created_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS credentials (name TEXT PRIMARY KEY, nonce BLOB NOT NULL, ciphertext BLOB NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, message_id TEXT, in_reply_to TEXT, reference_ids TEXT, thread_id TEXT NOT NULL, threading_scanned INTEGER NOT NULL DEFAULT 1, recipient_headers_scanned INTEGER NOT NULL DEFAULT 1, subject TEXT NOT NULL, from_name TEXT, from_address TEXT NOT NULL, to_addresses TEXT NOT NULL, cc_addresses TEXT NOT NULL DEFAULT '', bcc_addresses TEXT NOT NULL DEFAULT '', reply_to_addresses TEXT NOT NULL DEFAULT '', received_at TEXT NOT NULL, snippet TEXT NOT NULL, body_text TEXT NOT NULL, unsubscribe_kind TEXT, unsubscribe_url TEXT, unsubscribe_scanned INTEGER NOT NULL DEFAULT 0, is_read INTEGER NOT NULL DEFAULT 0, is_flagged INTEGER NOT NULL DEFAULT 0, is_answered INTEGER NOT NULL DEFAULT 0, is_draft INTEGER NOT NULL DEFAULT 0, has_attachments INTEGER NOT NULL DEFAULT 0, category TEXT, classification_confidence REAL, classification_source TEXT, classification_signals TEXT NOT NULL DEFAULT '', UNIQUE(account_id, mailbox, uid))",
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, from_name, from_address, to_addresses, body_text, content='messages', content_rowid='rowid')",
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE INDEX IF NOT EXISTS messages_account_mailbox_date ON messages(account_id, mailbox, received_at DESC)",
            // Local search keysets order an account-wide corpus by this exact
            // tuple. Without it a broad draft preview sorts every catalogue
            // row before the bounded LIMIT can take effect.
            "CREATE INDEX IF NOT EXISTS messages_account_received_id ON messages(account_id, received_at DESC, id DESC)",
            "CREATE TABLE IF NOT EXISTS mailbox_sync_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, initialized_at TEXT NOT NULL, highest_uid INTEGER, uid_validity INTEGER, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS selectable_mailboxes (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, remote_path TEXT NOT NULL, local_path TEXT NOT NULL, hierarchy_delimiter TEXT, parent_id TEXT REFERENCES selectable_mailboxes(id) ON DELETE SET NULL ON UPDATE CASCADE, parent_path TEXT, special_use TEXT, selectable INTEGER NOT NULL DEFAULT 1, uid_validity INTEGER, catalogue_coverage TEXT NOT NULL DEFAULT 'unknown' CHECK(catalogue_coverage IN ('unknown', 'partial', 'complete')), updated_at TEXT NOT NULL, UNIQUE(account_id, remote_path))",
            "CREATE INDEX IF NOT EXISTS selectable_mailboxes_account_local_path ON selectable_mailboxes(account_id, local_path, remote_path)",
            "CREATE TABLE IF NOT EXISTS message_mailbox_memberships (message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, mailbox_id TEXT NOT NULL REFERENCES selectable_mailboxes(id) ON DELETE CASCADE ON UPDATE CASCADE, account_id TEXT NOT NULL, PRIMARY KEY(message_id, mailbox_id))",
            "CREATE INDEX IF NOT EXISTS message_mailbox_memberships_account_message ON message_mailbox_memberships(account_id, message_id)",
            "CREATE TABLE IF NOT EXISTS mailbox_action_tombstones (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0, data BLOB NOT NULL)",
            "CREATE INDEX IF NOT EXISTS attachments_message_id ON attachments(message_id)",
            "CREATE TABLE IF NOT EXISTS message_attachment_catalogue (message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, attachment_id TEXT NOT NULL DEFAULT '', filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', PRIMARY KEY(message_id, attachment_id))",
            "CREATE INDEX IF NOT EXISTS message_attachment_catalogue_message_id ON message_attachment_catalogue(message_id)",
            "CREATE TABLE IF NOT EXISTS starred_message_bodies (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, body_text TEXT NOT NULL, body_html TEXT, attachment_presentation_version INTEGER NOT NULL DEFAULT 0, cached_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS starred_attachment_metadata (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS message_content_cache (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, content_state TEXT NOT NULL CHECK(content_state = 'complete'), body_text TEXT NOT NULL, body_html TEXT, unsubscribe_kind TEXT, attachments_json TEXT NOT NULL, byte_size INTEGER NOT NULL CHECK(byte_size >= 0), last_accessed INTEGER NOT NULL)",
            "CREATE INDEX IF NOT EXISTS message_content_cache_lru ON message_content_cache(last_accessed, message_id)",
            "CREATE TABLE IF NOT EXISTS message_search_body_text (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, body_text TEXT NOT NULL, byte_size INTEGER NOT NULL CHECK(byte_size >= 0), last_indexed INTEGER NOT NULL)",
            "CREATE INDEX IF NOT EXISTS message_search_body_text_lru ON message_search_body_text(last_indexed, message_id)",
            "CREATE TABLE IF NOT EXISTS message_content_fetches (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, claimed_at TEXT NOT NULL, claim_owner TEXT NOT NULL DEFAULT '')",
            "CREATE TABLE IF NOT EXISTS app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS opaque_mailbox_storage_identity_progress (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), last_account_id TEXT NOT NULL DEFAULT '', last_mailbox TEXT NOT NULL DEFAULT '', complete INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS opaque_mailbox_storage_identity_unresolved (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, noted_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS search_catalogue_v2_progress (stage TEXT PRIMARY KEY, last_rowid INTEGER NOT NULL DEFAULT 0, target_rowid INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS mailbox_catalog_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, remote_total INTEGER NOT NULL DEFAULT 0, historical_complete INTEGER NOT NULL DEFAULT 0, uid_next INTEGER, highest_modseq TEXT, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_generations (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, generation TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, initial_exists INTEGER NOT NULL, uid_next INTEGER, highest_modseq TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation))",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_items (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, is_read INTEGER NOT NULL, is_flagged INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_messages (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, message_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_replacement_outcomes (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, outcome TEXT NOT NULL CHECK(outcome IN ('message', 'excluded')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE INDEX IF NOT EXISTS mailbox_snapshot_items_generation_uid ON mailbox_snapshot_items(account_id, mailbox, generation, uid DESC)",
            "CREATE TABLE IF NOT EXISTS mailbox_sync_failures (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, stage TEXT NOT NULL, error TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS mail_rebuild_jobs (account_id TEXT PRIMARY KEY, phase TEXT NOT NULL, completed INTEGER NOT NULL DEFAULT 0, total INTEGER, reset_before_sync INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS deleted_account_tombstones (account_id TEXT PRIMARY KEY, deleted_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS sent_correspondents (account_id TEXT NOT NULL, address TEXT NOT NULL COLLATE NOCASE, PRIMARY KEY(account_id, address))",
            "CREATE TABLE IF NOT EXISTS account_search_generations (account_id TEXT PRIMARY KEY, generation INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS contacted_people (canonical_address TEXT PRIMARY KEY, display_name TEXT, formatted_address TEXT NOT NULL, first_contacted_at TEXT NOT NULL, last_contacted_at TEXT NOT NULL, send_count INTEGER NOT NULL CHECK(send_count >= 0), hidden_at TEXT, hidden_sequence INTEGER NOT NULL DEFAULT 0, normalized_display_name TEXT NOT NULL DEFAULT '', normalized_address TEXT NOT NULL DEFAULT '', normalized_display_tokens TEXT NOT NULL DEFAULT '', normalized_address_tokens TEXT NOT NULL DEFAULT '')",
            "CREATE TABLE IF NOT EXISTS contacted_people_account_stats (canonical_address TEXT NOT NULL REFERENCES contacted_people(canonical_address) ON DELETE CASCADE ON UPDATE CASCADE, account_id TEXT NOT NULL, first_contacted_at TEXT NOT NULL, last_contacted_at TEXT NOT NULL, send_count INTEGER NOT NULL CHECK(send_count >= 0), display_name TEXT, formatted_address TEXT, PRIMARY KEY(canonical_address, account_id))",
            "CREATE INDEX IF NOT EXISTS contacted_people_account_recency ON contacted_people_account_stats(account_id, last_contacted_at DESC, canonical_address)",
            "CREATE INDEX IF NOT EXISTS contacted_people_visible_global_rank ON contacted_people(send_count DESC, last_contacted_at DESC, canonical_address) WHERE hidden_at IS NULL",
            "CREATE INDEX IF NOT EXISTS contacted_people_visible_recent ON contacted_people(last_contacted_at DESC, canonical_address) WHERE hidden_at IS NULL",
            "CREATE INDEX IF NOT EXISTS contacted_people_visible_normalized_address ON contacted_people(normalized_address) WHERE hidden_at IS NULL",
            "CREATE INDEX IF NOT EXISTS contacted_people_normalized_migration ON contacted_people(normalized_address)",
            "CREATE TABLE IF NOT EXISTS contacted_people_backfill_progress (account_id TEXT PRIMARY KEY, processed_messages INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS contacted_people_backfill_messages (account_id TEXT NOT NULL, message_id TEXT NOT NULL, PRIMARY KEY(account_id, message_id))",
            "CREATE TABLE IF NOT EXISTS contacted_people_legacy_unresolved_sources (account_id TEXT NOT NULL, message_id TEXT NOT NULL, PRIMARY KEY(account_id, message_id))",
            "CREATE INDEX IF NOT EXISTS contacted_people_backfill_messages_message ON contacted_people_backfill_messages(message_id)",
            "CREATE TABLE IF NOT EXISTS contacted_people_backfill_sources (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid_validity INTEGER NOT NULL, uid INTEGER NOT NULL, PRIMARY KEY(account_id, mailbox, uid_validity, uid))",
            "CREATE TABLE IF NOT EXISTS contacted_people_backfill_rfc_messages (account_id TEXT NOT NULL, rfc_message_id TEXT NOT NULL, PRIMARY KEY(account_id, rfc_message_id))",
            "CREATE TABLE IF NOT EXISTS contacted_people_outgoing_messages (account_id TEXT NOT NULL, rfc_message_id TEXT NOT NULL, recorded_at TEXT NOT NULL, PRIMARY KEY(account_id, rfc_message_id))",
            "CREATE INDEX IF NOT EXISTS contacted_people_outgoing_messages_account ON contacted_people_outgoing_messages(account_id, recorded_at)",
            "CREATE TABLE IF NOT EXISTS contacted_people_sent_provider_cutoffs (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation INTEGER NOT NULL, uid_validity INTEGER NOT NULL, cutoff_uid INTEGER NOT NULL, captured_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation))",
            "CREATE INDEX IF NOT EXISTS contacted_people_sent_provider_cutoffs_account_generation ON contacted_people_sent_provider_cutoffs(account_id, generation, mailbox)",
        ] {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("storage migration statement failed: {statement}"))?;
        }
        let fetch_claim_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(message_content_fetches)")
                .fetch_all(&self.pool)
                .await?;
        if !fetch_claim_columns
            .iter()
            .any(|column| column.1 == "claim_owner")
        {
            sqlx::query(
                "ALTER TABLE message_content_fetches ADD COLUMN claim_owner TEXT NOT NULL DEFAULT ''",
            )
            .execute(&self.pool)
            .await?;
        }
        let catalog_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mailbox_catalog_state)")
                .fetch_all(&self.pool)
                .await?;
        if !catalog_columns.iter().any(|column| column.1 == "uid_next") {
            sqlx::query("ALTER TABLE mailbox_catalog_state ADD COLUMN uid_next INTEGER")
                .execute(&self.pool)
                .await?;
        }
        if !catalog_columns
            .iter()
            .any(|column| column.1 == "highest_modseq")
        {
            sqlx::query("ALTER TABLE mailbox_catalog_state ADD COLUMN highest_modseq TEXT")
                .execute(&self.pool)
                .await?;
        }
        let contacted_people_stats_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(contacted_people_account_stats)")
                .fetch_all(&self.pool)
                .await?;
        for (name, definition) in [("display_name", "TEXT"), ("formatted_address", "TEXT")] {
            if !contacted_people_stats_columns
                .iter()
                .any(|column| column.1 == name)
            {
                sqlx::query(&format!(
                    "ALTER TABLE contacted_people_account_stats ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        let contacted_people_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(contacted_people)")
                .fetch_all(&self.pool)
                .await?;
        for name in [
            "normalized_display_name",
            "normalized_address",
            "normalized_display_tokens",
            "normalized_address_tokens",
        ] {
            if !contacted_people_columns
                .iter()
                .any(|column| column.1 == name)
            {
                sqlx::query(&format!(
                    "ALTER TABLE contacted_people ADD COLUMN {name} TEXT NOT NULL DEFAULT ''"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        if !contacted_people_columns
            .iter()
            .any(|column| column.1 == "hidden_sequence")
        {
            sqlx::query("ALTER TABLE contacted_people ADD COLUMN hidden_sequence INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await?;
        }
        // Start legacy contacted-people upgrades, but only with one bounded
        // batch. The desktop startup worker resumes any remainder after open.
        self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
            .await?;
        // Previous builds represented Clear as a permanent provider-backfill
        // ban. Preserve its privacy guarantee while upgrading to a precise
        // provider-identity cutoff: start at generation one and fail closed
        // until the Sent SELECT hook captures each mailbox boundary.
        sqlx::query("INSERT INTO app_meta(key, value) SELECT 'contacted_people_collection_generation', '1' WHERE EXISTS (SELECT 1 FROM app_meta WHERE key = 'contacted_people_cleared_at') AND NOT EXISTS (SELECT 1 FROM app_meta WHERE key = 'contacted_people_collection_generation')")
            .execute(&self.pool)
            .await?;
        // A CLI and the desktop can open this database concurrently. Preserve
        // every fresh lease regardless of process owner; only work old enough
        // to have outlived the foreground wait window is safe to discard.
        sqlx::query("DELETE FROM message_content_fetches WHERE claimed_at <= ?")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS))
            .execute(&self.pool)
            .await?;
        // Provider work can outlive a UI action (for example, a hydration
        // task that fetched an IMAP message just before the account was
        // removed). These are deliberately database-level guards rather
        // than a best-effort caller convention: every late account-scoped
        // insert is rejected once the account's deletion transaction commits.
        for statement in [
            "CREATE TRIGGER IF NOT EXISTS accounts_require_new_identity BEFORE INSERT ON accounts WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS accounts_updates_require_live_identity BEFORE UPDATE ON accounts WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS messages_require_account BEFORE INSERT ON messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_sync_state_require_account BEFORE INSERT ON mailbox_sync_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS selectable_mailboxes_require_account BEFORE INSERT ON selectable_mailboxes WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS message_mailbox_memberships_require_account BEFORE INSERT ON message_mailbox_memberships WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_action_tombstones_require_account BEFORE INSERT ON mailbox_action_tombstones WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_catalog_state_require_account BEFORE INSERT ON mailbox_catalog_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_generations_require_account BEFORE INSERT ON mailbox_snapshot_generations WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_items_require_account BEFORE INSERT ON mailbox_snapshot_items WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_messages_require_account BEFORE INSERT ON mailbox_snapshot_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_replacement_outcomes_require_account BEFORE INSERT ON mailbox_snapshot_replacement_outcomes WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_sync_failures_require_account BEFORE INSERT ON mailbox_sync_failures WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mail_rebuild_jobs_require_account BEFORE INSERT ON mail_rebuild_jobs WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS sent_correspondents_require_account BEFORE INSERT ON sent_correspondents WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_stats_require_account BEFORE INSERT ON contacted_people_account_stats WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_backfill_progress_require_account BEFORE INSERT ON contacted_people_backfill_progress WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_backfill_messages_require_account BEFORE INSERT ON contacted_people_backfill_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_legacy_unresolved_sources_require_account BEFORE INSERT ON contacted_people_legacy_unresolved_sources WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_backfill_sources_require_account BEFORE INSERT ON contacted_people_backfill_sources WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_backfill_rfc_messages_require_account BEFORE INSERT ON contacted_people_backfill_rfc_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_outgoing_messages_require_account BEFORE INSERT ON contacted_people_outgoing_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS contacted_people_sent_provider_cutoffs_require_account BEFORE INSERT ON contacted_people_sent_provider_cutoffs WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
        ] {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("account deletion guard migration failed: {statement}"))?;
        }
        self.cleanup_orphaned_account_state().await?;
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(messages)")
                .fetch_all(&self.pool)
                .await?;
        let thread_schema_changed = ["in_reply_to", "reference_ids", "thread_id"]
            .iter()
            .any(|name| !columns.iter().any(|column| column.1 == *name));
        if !columns.iter().any(|column| column.1 == "body_html") {
            sqlx::query("ALTER TABLE messages ADD COLUMN body_html TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !columns.iter().any(|column| column.1 == "content_state") {
            sqlx::query(
                "ALTER TABLE messages ADD COLUMN content_state TEXT NOT NULL DEFAULT 'complete'",
            )
            .execute(&self.pool)
            .await?;
        }
        let rebuild_job_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mail_rebuild_jobs)")
                .fetch_all(&self.pool)
                .await?;
        if !rebuild_job_columns
            .iter()
            .any(|column| column.1 == "reset_before_sync")
        {
            sqlx::query(
                "ALTER TABLE mail_rebuild_jobs ADD COLUMN reset_before_sync INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await?;
        }
        for name in ["cc_addresses", "bcc_addresses", "reply_to_addresses"] {
            if !columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE messages ADD COLUMN {name} TEXT NOT NULL DEFAULT ''"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        if !columns
            .iter()
            .any(|column| column.1 == "recipient_headers_scanned")
        {
            // Existing local rows predate durable recipient metadata. Mark
            // them pending even though the new columns themselves default to
            // empty: an empty Cc/Bcc/Reply-To is only authoritative after a
            // provider header fetch has observed it.
            sqlx::query(
                "ALTER TABLE messages ADD COLUMN recipient_headers_scanned INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS messages_recipient_header_backfill ON messages(account_id, mailbox, recipient_headers_scanned, received_at DESC)")
            .execute(&self.pool)
            .await?;
        let sync_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mailbox_sync_state)")
                .fetch_all(&self.pool)
                .await?;
        if !sync_columns.iter().any(|column| column.1 == "highest_uid") {
            sqlx::query("ALTER TABLE mailbox_sync_state ADD COLUMN highest_uid INTEGER")
                .execute(&self.pool)
                .await?;
        }
        if !sync_columns.iter().any(|column| column.1 == "uid_validity") {
            sqlx::query("ALTER TABLE mailbox_sync_state ADD COLUMN uid_validity INTEGER")
                .execute(&self.pool)
                .await?;
        }
        sqlx::query("UPDATE mailbox_sync_state SET highest_uid = (SELECT MAX(uid) FROM messages WHERE messages.account_id = mailbox_sync_state.account_id AND messages.mailbox = mailbox_sync_state.mailbox) WHERE highest_uid IS NULL")
            .execute(&self.pool)
            .await?;
        if !columns.iter().any(|column| column.1 == "in_reply_to") {
            sqlx::query("ALTER TABLE messages ADD COLUMN in_reply_to TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !columns.iter().any(|column| column.1 == "reference_ids") {
            sqlx::query("ALTER TABLE messages ADD COLUMN reference_ids TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !columns.iter().any(|column| column.1 == "thread_id") {
            sqlx::query("ALTER TABLE messages ADD COLUMN thread_id TEXT")
                .execute(&self.pool)
                .await?;
        }
        // An interrupted legacy migration can leave the nullable column in
        // place before its backfill commits. Repair those rows on every open,
        // before indexes or thread rebuilding attempt to read the value.
        sqlx::query("UPDATE messages SET thread_id = id WHERE thread_id IS NULL")
            .execute(&self.pool)
            .await?;
        if !columns.iter().any(|column| column.1 == "threading_scanned") {
            // Rows that predate threading support must be header-backfilled.
            // New rows are written with threading_scanned=1 by persist_message.
            sqlx::query(
                "ALTER TABLE messages ADD COLUMN threading_scanned INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS messages_account_thread ON messages(account_id, thread_id, received_at)")
            .execute(&self.pool)
            .await?;
        for statement in [
            "CREATE INDEX IF NOT EXISTS messages_smart_representative ON messages(account_id, mailbox, thread_id, received_at DESC, id DESC)",
            "CREATE INDEX IF NOT EXISTS messages_thread_flagged ON messages(account_id, thread_id) WHERE is_flagged = 1",
            "CREATE INDEX IF NOT EXISTS messages_thread_unread ON messages(account_id, thread_id, mailbox) WHERE is_read = 0",
        ] {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS messages_threading_backfill ON messages(account_id, mailbox, threading_scanned, received_at DESC)")
            .execute(&self.pool)
            .await?;
        if !columns.iter().any(|column| column.1 == "unsubscribe_kind") {
            sqlx::query("ALTER TABLE messages ADD COLUMN unsubscribe_kind TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !columns.iter().any(|column| column.1 == "unsubscribe_url") {
            sqlx::query("ALTER TABLE messages ADD COLUMN unsubscribe_url TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !columns
            .iter()
            .any(|column| column.1 == "unsubscribe_scanned")
        {
            sqlx::query(
                "ALTER TABLE messages ADD COLUMN unsubscribe_scanned INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await?;
        }
        for (name, definition) in [
            ("category", "TEXT"),
            ("classification_confidence", "REAL"),
            ("classification_source", "TEXT"),
            ("classification_signals", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE messages ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        self.initialize_classification_policy().await?;
        for (name, definition) in [
            ("is_answered", "INTEGER NOT NULL DEFAULT 0"),
            ("is_draft", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE messages ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        self.migrate_attachment_presentation_metadata().await?;
        if !sync_state_exists {
            sqlx::query("INSERT OR IGNORE INTO mailbox_sync_state(account_id, mailbox, initialized_at) SELECT id, 'INBOX', ? FROM accounts")
                .bind(Utc::now())
                .execute(&self.pool)
                .await?;
        }
        if migrated_legacy_profile {
            self.restore_legacy_desktop_profile().await?;
        }
        // Legacy Sent rows are restored above. Backfill only after every
        // source of existing messages has run, otherwise the one-time version
        // marker would permanently skip those correspondents.
        self.migrate_sent_correspondents().await?;
        // Replace the legacy body FTS before any migration-time message
        // updates. External-content FTS triggers must match the active table
        // definition or SQLite can report a malformed database.
        self.migrate_to_metadata_catalogue().await?;
        self.migrate_selectable_mailboxes().await?;
        self.migrate_opaque_mailbox_storage_identities().await?;
        self.migrate_search_catalogue_v2().await?;
        self.migrate_contacted_people_enabled_aggregates(
            CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32,
        )
        .await?;
        if thread_schema_changed {
            let account_ids: Vec<String> =
                sqlx::query_scalar("SELECT DISTINCT account_id FROM messages")
                    .fetch_all(&self.pool)
                    .await?;
            for account_id in account_ids {
                self.rebuild_threads_for_account(&account_id).await?;
            }
        }
        Ok(())
    }

    /// Fresh databases need a policy value so later write reservations have a
    /// stable row. Existing values are not changed here: upgrades are guarded
    /// by the owner-scoped classifier claim below.
    async fn initialize_classification_policy(&self) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO app_meta(key, value) VALUES ('classification_policy_version', ?)")
            .bind(CLASSIFICATION_POLICY_VERSION)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Claims or renews the current classifier model and policy for one drain.
    /// A different live owner fails closed; a clean owner releases immediately
    /// and the activity timeout remains only for crash recovery.
    pub async fn claim_classification_revision(&self, owner: &str, revision: &str) -> Result<()> {
        if owner.trim().is_empty() {
            return Err(anyhow!("classification owner cannot be empty"));
        }
        if revision.trim().is_empty() {
            return Err(anyhow!("classification model revision cannot be empty"));
        }
        let mut transaction = self.pool.begin().await?;
        // Serialize the read/compare/update across CLI and desktop processes.
        sqlx::query(
            "UPDATE app_meta SET value = value WHERE key = 'classification_policy_version'",
        )
        .execute(&mut *transaction)
        .await?;
        let current_model: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'classification_model_revision'",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let current_policy: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'classification_policy_version'",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let current_owner: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'classification_revision_owner'",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let last_activity: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'classification_revision_active_at'",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let now = Utc::now().timestamp();
        let another_owner_is_active = current_owner.as_deref().is_some_and(|value| value != owner)
            && last_activity
                .as_deref()
                .and_then(|value| value.parse::<i64>().ok())
                .is_some_and(|timestamp| {
                    now.saturating_sub(timestamp) < CLASSIFICATION_REVISION_ACTIVITY_SECONDS
                });
        if another_owner_is_active {
            return Err(anyhow!(
                "another email classifier is active; retry after it finishes"
            ));
        }
        let revisions_differ = current_model.as_deref() != Some(revision)
            || current_policy.as_deref() != Some(CLASSIFICATION_POLICY_VERSION);
        if revisions_differ {
            sqlx::query("UPDATE messages SET classification_source = NULL, classification_confidence = NULL WHERE classification_source = 'model'")
                .execute(&mut *transaction)
                .await?;
            sqlx::query("INSERT INTO app_meta(key, value) VALUES ('classification_model_revision', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                .bind(revision)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("INSERT INTO app_meta(key, value) VALUES ('classification_policy_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                .bind(CLASSIFICATION_POLICY_VERSION)
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('classification_revision_active_at', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(now.to_string())
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('classification_revision_owner', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(owner)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn release_classification_revision(&self, owner: &str) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE app_meta SET value = value WHERE key = 'classification_policy_version'",
        )
        .execute(&mut *transaction)
        .await?;
        let current_owner: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'classification_revision_owner'",
        )
        .fetch_optional(&mut *transaction)
        .await?;
        if current_owner.as_deref() == Some(owner) {
            sqlx::query("DELETE FROM app_meta WHERE key IN ('classification_revision_owner', 'classification_revision_active_at')")
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn migrate_sent_correspondents(&self) -> Result<()> {
        let version: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'sent_correspondents_version'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if version.as_deref() == Some("1") {
            return Ok(());
        }

        let sent_headers = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT account_id, to_addresses, cc_addresses, bcc_addresses FROM messages WHERE mailbox = 'Sent' OR mailbox LIKE 'Sent::%'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut correspondents = HashSet::new();
        for (account_id, to, cc, bcc) in sent_headers {
            for address in [to, cc, bcc]
                .iter()
                .flat_map(|header| crate::mail::parsed_header_mailboxes(header))
            {
                correspondents.insert((account_id.clone(), address.to_lowercase()));
            }
        }
        let mut tx = self.pool.begin().await?;
        for (account_id, address) in correspondents {
            sqlx::query(
                "INSERT OR IGNORE INTO sent_correspondents(account_id, address) VALUES (?, ?)",
            )
            .bind(account_id)
            .bind(address)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('sent_correspondents_version', '1') ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Converts legacy all-account denormalized person fields to enabled-only
    /// values. Account stats retain disabled contributions, so re-enabling an
    /// account can restore them without re-learning mail.
    async fn migrate_contacted_people_enabled_aggregates(
        &self,
        limit: u32,
    ) -> Result<ContactedPeopleMigrationBatch> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if contacted_people_migration_complete_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY,
        )
        .await?
        {
            tx.commit().await?;
            return Ok(ContactedPeopleMigrationBatch {
                processed: 0,
                complete: true,
            });
        }
        let cursor = contacted_people_migration_cursor_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY,
        )
        .await?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT rowid, canonical_address FROM contacted_people WHERE rowid > ? ORDER BY rowid LIMIT ?",
        )
        .bind(cursor)
        .bind(i64::from(limit.clamp(1, CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)))
        .fetch_all(&mut *tx)
        .await?;
        for (_, address) in &rows {
            recompute_enabled_contacted_people_aggregate_for_in_tx(&mut tx, address).await?;
        }
        let last_rowid = rows.last().map(|row| row.0);
        let has_more = if let Some(last_rowid) = last_rowid {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM contacted_people WHERE rowid > ?)")
                .bind(last_rowid)
                .fetch_one(&mut *tx)
                .await?
        } else {
            false
        };
        finish_contacted_people_migration_batch_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY,
            CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY,
            last_rowid,
            has_more,
        )
        .await?;
        tx.commit().await?;
        Ok(ContactedPeopleMigrationBatch {
            processed: rows.len(),
            complete: !has_more,
        })
    }
    /// Legacy attachment rows only record MIME transport disposition. Rather
    /// than guessing whether a CID/logo is a user-facing file, discard stale
    /// attachment metadata and let the next authoritative MIME fetch classify
    /// the selected HTML branch. This also prevents stale paperclips from
    /// surviving the schema upgrade.
    async fn migrate_attachment_presentation_metadata(&self) -> Result<()> {
        for table in [
            "attachments",
            "starred_attachment_metadata",
            "message_attachment_catalogue",
        ] {
            let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
                sqlx::query_as(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&self.pool)
                    .await?;
            if !columns.iter().any(|column| column.1 == "presentation") {
                sqlx::query(&format!(
                    "ALTER TABLE {table} ADD COLUMN presentation TEXT NOT NULL DEFAULT 'unknown'"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        self.migrate_message_attachment_catalogue_identity().await?;
        let starred_body_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(starred_message_bodies)")
                .fetch_all(&self.pool)
                .await?;
        if !starred_body_columns
            .iter()
            .any(|column| column.1 == "attachment_presentation_version")
        {
            sqlx::query("ALTER TABLE starred_message_bodies ADD COLUMN attachment_presentation_version INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await?;
        }

        let version: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'attachment_presentation_cache_version'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if version.as_deref() == Some(ATTACHMENT_PRESENTATION_CACHE_VERSION) {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        for statement in [
            "DELETE FROM attachments",
            "DELETE FROM starred_attachment_metadata",
            "DELETE FROM message_content_cache",
            "UPDATE messages SET has_attachments = 0",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('attachment_presentation_cache_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(ATTACHMENT_PRESENTATION_CACHE_VERSION)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The original metadata primary key collapsed distinct MIME parts with
    /// identical visible metadata. Preserve each opaque part identity instead
    /// of merging its presentation into a synthetic third state. Legacy rows
    /// receive a stable rowid-derived identity because no stronger part ID was
    /// persisted by those builds.
    async fn migrate_message_attachment_catalogue_identity(&self) -> Result<()> {
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(message_attachment_catalogue)")
                .fetch_all(&self.pool)
                .await?;
        if columns.iter().any(|column| column.1 == "attachment_id") {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for trigger in [
            "message_attachment_catalogue_ai",
            "starred_attachment_catalogue_ai",
        ] {
            sqlx::query(&format!("DROP TRIGGER IF EXISTS {trigger}"))
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("CREATE TABLE message_attachment_catalogue_rebuilt (message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, attachment_id TEXT NOT NULL, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', PRIMARY KEY(message_id, attachment_id))")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO message_attachment_catalogue_rebuilt(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT message_id, 'legacy:' || rowid, filename, mime_type, size_bytes, is_inline, presentation FROM message_attachment_catalogue")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE message_attachment_catalogue")
            .execute(&mut *tx)
            .await?;
        sqlx::query("ALTER TABLE message_attachment_catalogue_rebuilt RENAME TO message_attachment_catalogue")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS message_attachment_catalogue_message_id ON message_attachment_catalogue(message_id)")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Repairs databases written by builds that could delete `accounts` while
    /// provider work was still publishing.  Tombstone every discovered
    /// orphan in the same transaction before deleting its local state, so a
    /// subsequent stale writer cannot recreate it after the repair.
    async fn cleanup_orphaned_account_state(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT OR IGNORE INTO deleted_account_tombstones(account_id, deleted_at) \
             SELECT orphan.account_id, ? FROM ( \
                 SELECT account_id FROM messages \
                 UNION SELECT account_id FROM mailbox_sync_state \
                 UNION SELECT account_id FROM selectable_mailboxes \
                 UNION SELECT account_id FROM message_mailbox_memberships \
                 UNION SELECT account_id FROM mailbox_catalog_state \
                 UNION SELECT account_id FROM mailbox_snapshot_generations \
                 UNION SELECT account_id FROM mailbox_sync_failures \
                 UNION SELECT account_id FROM mailbox_action_tombstones \
                 UNION SELECT account_id FROM mail_rebuild_jobs \
                 UNION SELECT account_id FROM sent_correspondents \
                 UNION SELECT account_id FROM contacted_people_account_stats \
                 UNION SELECT account_id FROM contacted_people_backfill_progress \
                 UNION SELECT account_id FROM contacted_people_backfill_messages \
                 UNION SELECT account_id FROM contacted_people_legacy_unresolved_sources \
                 UNION SELECT account_id FROM contacted_people_backfill_sources \
                 UNION SELECT account_id FROM contacted_people_backfill_rfc_messages \
                 UNION SELECT account_id FROM contacted_people_outgoing_messages \
                 UNION SELECT account_id FROM contacted_people_sent_provider_cutoffs \
             ) AS orphan \
             WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE id = orphan.account_id)",
        )
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;
        // Deleting an orphan account's stats directly leaves the denormalized
        // global person row with that account's name, count, and timestamps.
        // Use the same aggregate-rebuild path as an intentional account
        // deletion before the generic orphan cleanup below.
        let orphan_people_accounts: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT stats.account_id FROM contacted_people_account_stats stats WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = stats.account_id)",
        )
        .fetch_all(&mut *tx)
        .await?;
        for account_id in orphan_people_accounts {
            remove_contacted_people_account_contribution_in_tx(&mut tx, &account_id).await?;
        }
        // Delete message dependents explicitly before their parent. Modern
        // schema revisions also cascade these rows, but explicit cleanup
        // repairs older local schemas that may not have had those FKs.
        for statement in [
            "DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id)",
            "DELETE FROM message_mailbox_memberships WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = message_mailbox_memberships.account_id)",
            "DELETE FROM selectable_mailboxes WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = selectable_mailboxes.account_id)",
            "DELETE FROM mailbox_sync_state WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_sync_state.account_id)",
            "DELETE FROM mailbox_catalog_state WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_catalog_state.account_id)",
            "DELETE FROM mailbox_snapshot_generations WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_snapshot_generations.account_id)",
            "DELETE FROM mailbox_sync_failures WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_sync_failures.account_id)",
            "DELETE FROM mailbox_action_tombstones WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_action_tombstones.account_id)",
            "DELETE FROM mail_rebuild_jobs WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mail_rebuild_jobs.account_id)",
            "DELETE FROM sent_correspondents WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = sent_correspondents.account_id)",
            "DELETE FROM contacted_people_account_stats WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_account_stats.account_id)",
            "DELETE FROM contacted_people_backfill_progress WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_backfill_progress.account_id)",
            "DELETE FROM contacted_people_backfill_messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_backfill_messages.account_id)",
            "DELETE FROM contacted_people_legacy_unresolved_sources WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_legacy_unresolved_sources.account_id)",
            "DELETE FROM contacted_people_backfill_sources WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_backfill_sources.account_id)",
            "DELETE FROM contacted_people_backfill_rfc_messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_backfill_rfc_messages.account_id)",
            "DELETE FROM contacted_people_outgoing_messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_outgoing_messages.account_id)",
            "DELETE FROM contacted_people_sent_provider_cutoffs WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = contacted_people_sent_provider_cutoffs.account_id)",
            "DELETE FROM contacted_people WHERE NOT EXISTS (SELECT 1 FROM contacted_people_account_stats WHERE contacted_people_account_stats.canonical_address = contacted_people.canonical_address)",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Continues the one-time contacted-people upgrades in durable, bounded
    /// transactions. `open` performs one 500-row starter batch; a background
    /// caller should yield between calls until `complete` is true. Neither
    /// upgrade changes recipient statistics, so callers must not emit a
    /// contacted-people data-change event merely for this progress.
    pub async fn continue_contacted_people_migrations(
        &self,
        limit: u32,
    ) -> Result<ContactedPeopleMigrationProgress> {
        let limit = i64::from(limit.clamp(1, CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32));
        let normalized = self
            .migrate_contacted_people_normalized_search(limit)
            .await?;
        let sources = self
            .migrate_contacted_people_backfill_sources(limit)
            .await?;
        let active_aggregates = self
            .migrate_contacted_people_enabled_aggregates(limit as u32)
            .await?;
        Ok(ContactedPeopleMigrationProgress {
            normalized_people: normalized.processed,
            source_markers: sources.processed,
            changed_people: 0,
            complete: normalized.complete && sources.complete && active_aggregates.complete,
        })
    }

    /// Backfills additive normalized fields for profiles created before
    /// contacted-people search was indexed. Normalization is intentionally
    /// performed in Rust so it has the exact same Unicode and diacritic
    /// semantics as live writes and autocomplete queries.
    async fn migrate_contacted_people_normalized_search(
        &self,
        limit: i64,
    ) -> Result<ContactedPeopleMigrationBatch> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if contacted_people_migration_complete_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_COMPLETE_KEY,
        )
        .await?
        {
            tx.commit().await?;
            return Ok(ContactedPeopleMigrationBatch {
                processed: 0,
                complete: true,
            });
        }
        let cursor = contacted_people_migration_cursor_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_CURSOR_KEY,
        )
        .await?;
        let people: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT rowid, canonical_address, display_name FROM contacted_people WHERE normalized_address = '' AND rowid > ? ORDER BY rowid LIMIT ?",
        )
        .bind(cursor)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let last_rowid = people.last().map(|row| row.0);
        for (_, address, display_name) in &people {
            update_contacted_people_normalized_search_in_tx(
                &mut tx,
                address,
                display_name.as_deref(),
            )
            .await?;
        }
        let has_more = match last_rowid {
            Some(last_rowid) => sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM contacted_people WHERE normalized_address = '' AND rowid > ?)",
            )
            .bind(last_rowid)
            .fetch_one(&mut *tx)
            .await?,
            None => false,
        };
        finish_contacted_people_migration_batch_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_CURSOR_KEY,
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_COMPLETE_KEY,
            last_rowid,
            has_more,
        )
        .await?;
        tx.commit().await?;
        Ok(ContactedPeopleMigrationBatch {
            processed: people.len(),
            complete: !has_more,
        })
    }

    /// Upgrades legacy account/message-id markers to provider-safe source
    /// identities while their current mailbox identity is still available.
    /// The legacy table is the scan driver, including rows whose message has
    /// already been evicted: advancing past an unresolvable row prevents a
    /// restart loop while deliberately not treating it as a future UID match.
    async fn migrate_contacted_people_backfill_sources(
        &self,
        limit: i64,
    ) -> Result<ContactedPeopleMigrationBatch> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if contacted_people_migration_complete_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY,
        )
        .await?
        {
            tx.commit().await?;
            return Ok(ContactedPeopleMigrationBatch {
                processed: 0,
                complete: true,
            });
        }
        let cursor = contacted_people_migration_cursor_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY,
        )
        .await?;
        let rows: Vec<ContactedPeopleSourceMigrationRow> =
            sqlx::query_as(
                "SELECT legacy.rowid, legacy.account_id, legacy.message_id, m.mailbox, c.uid_validity, m.uid, m.message_id FROM contacted_people_backfill_messages legacy LEFT JOIN messages m ON m.account_id = legacy.account_id AND m.id = legacy.message_id LEFT JOIN mailbox_catalog_state c ON c.account_id = m.account_id AND c.mailbox = m.mailbox WHERE legacy.rowid > ? ORDER BY legacy.rowid LIMIT ?",
            )
            .bind(cursor)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
        let last_rowid = rows.last().map(|row| row.0);
        for (_, account_id, legacy_message_id, mailbox, uid_validity, uid, message_id) in &rows {
            let (Some(mailbox), Some(uid)) = (mailbox.as_deref(), uid) else {
                if let Some((mailbox, uid, is_v2)) =
                    legacy_contacted_people_marker_locator(account_id, legacy_message_id)
                {
                    if let Some((target, uid_validity)) =
                        resolve_evicted_legacy_contacted_people_source_in_tx(
                            &mut tx, account_id, &mailbox, is_v2,
                        )
                        .await?
                    {
                        sqlx::query("INSERT OR IGNORE INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) VALUES (?, ?, ?, ?)")
                            .bind(account_id)
                            .bind(target)
                            .bind(uid_validity.unwrap_or(-1))
                            .bind(uid)
                            .execute(&mut *tx)
                            .await?;
                        continue;
                    }
                }
                sqlx::query("INSERT OR IGNORE INTO contacted_people_legacy_unresolved_sources(account_id, message_id) VALUES (?, ?)")
                    .bind(account_id)
                    .bind(legacy_message_id)
                    .execute(&mut *tx)
                    .await?;
                continue;
            };
            sqlx::query("INSERT OR IGNORE INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) VALUES (?, ?, ?, ?)")
                .bind(account_id)
                .bind(mailbox)
                .bind(uid_validity.unwrap_or(-1))
                .bind(uid)
                .execute(&mut *tx)
                .await?;
            if let Some(rfc_message_id) = message_id.as_deref().and_then(normalize_message_id) {
                sqlx::query("INSERT OR IGNORE INTO contacted_people_backfill_rfc_messages(account_id, rfc_message_id) VALUES (?, ?)")
                    .bind(account_id)
                    .bind(rfc_message_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let has_more = match last_rowid {
            Some(last_rowid) => sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM contacted_people_backfill_messages WHERE rowid > ?)",
            )
            .bind(last_rowid)
            .fetch_one(&mut *tx)
            .await?,
            None => false,
        };
        finish_contacted_people_migration_batch_in_tx(
            &mut tx,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY,
            last_rowid,
            has_more,
        )
        .await?;
        tx.commit().await?;
        Ok(ContactedPeopleMigrationBatch {
            processed: rows.len(),
            complete: !has_more,
        })
    }

    /// Dakia's first desktop build stored a relational Electron profile using
    /// camelCase column names.  The current desktop and CLI intentionally use
    /// one JSON-account/catalogue store, so migrate that old profile before
    /// creating any current-schema indexes.  This keeps an existing desktop
    /// install searchable instead of failing on `messages.account_id`.
    async fn prepare_legacy_desktop_profile(&self) -> Result<bool> {
        let accounts_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'accounts')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !accounts_exists {
            return Ok(false);
        }
        let account_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(accounts)")
                .fetch_all(&self.pool)
                .await?;
        if account_columns.iter().any(|column| column.1 == "data") {
            return Ok(false);
        }
        let messages_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !messages_exists {
            return Ok(false);
        }
        let message_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(messages)")
                .fetch_all(&self.pool)
                .await?;
        if !message_columns.iter().any(|column| column.1 == "accountId") {
            return Ok(false);
        }

        let mut tx = self.pool.begin().await?;
        for statement in [
            "DROP TRIGGER IF EXISTS messages_ai",
            "DROP TRIGGER IF EXISTS messages_ad",
            "DROP TRIGGER IF EXISTS messages_au",
            "DROP TABLE IF EXISTS messages_fts",
            "DROP TABLE IF EXISTS message_search",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        for table in [
            "accounts",
            "messages",
            "mailboxes",
            "message_bodies",
            "sync_state",
            "threads",
            "thread_messages",
            "local_drafts",
            "operation_queue",
            "audit_log",
        ] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
            )
            .bind(table)
            .fetch_one(&mut *tx)
            .await?;
            if exists {
                sqlx::query(&format!("ALTER TABLE {table} RENAME TO legacy_{table}"))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn restore_legacy_desktop_profile(&self) -> Result<()> {
        let accounts: Vec<LegacyAccountRow> = sqlx::query_as(
            "SELECT id, email, displayName AS display_name, host, port, tls, username, providerCapabilities AS provider_capabilities, createdAt AS created_at FROM legacy_accounts ORDER BY createdAt",
        )
        .fetch_all(&self.pool)
        .await?;
        for legacy in accounts {
            let id = uuid::Uuid::parse_str(&legacy.id)
                .with_context(|| format!("legacy account {} has an invalid ID", legacy.email))?;
            let preset = provider::all()
                .iter()
                .find(|preset| preset.imap_host.eq_ignore_ascii_case(&legacy.host))
                .unwrap_or_else(|| provider::detect(&legacy.email));
            let capabilities: serde_json::Value =
                serde_json::from_str(&legacy.provider_capabilities)
                    .unwrap_or(serde_json::Value::Null);
            let archive_mailbox = legacy_capability(&capabilities, "archiveMailbox")
                .unwrap_or_else(|| preset.archive_mailbox.to_owned());
            let spam_mailbox = legacy_capability(&capabilities, "spamMailbox")
                .unwrap_or_else(|| preset.spam_mailbox.to_owned());
            let account_name = legacy.email.clone();
            let account = Account {
                id,
                email: legacy.email.clone(),
                account_name: account_name.clone(),
                display_name: legacy.display_name.unwrap_or(account_name),
                provider_id: preset.id.to_owned(),
                auth: AccountAuth::Password {
                    username: legacy.username,
                },
                imap_host: legacy.host,
                imap_port: u16::try_from(legacy.port)
                    .context("legacy account has an invalid IMAP port")?,
                imap_security: if legacy.tls != 0 {
                    provider::Security::Tls
                } else {
                    provider::Security::StartTls
                },
                smtp_host: preset.smtp_host.to_owned(),
                smtp_port: preset.smtp_port,
                smtp_security: preset.smtp_security,
                archive_mailbox,
                spam_mailbox,
                enabled: true,
                created_at: legacy_created_at(&legacy.created_at),
            };
            self.save_account(&account).await?;
        }

        let states: Vec<LegacyMailboxRow> = sqlx::query_as(
            "SELECT accountId AS account_id, path AS mailbox, path AS remote_name, uidValidity AS uid_validity FROM legacy_mailboxes",
        )
        .fetch_all(&self.pool)
        .await?;
        for state in states {
            let Some(uid_validity) = state.uid_validity else {
                continue;
            };
            sqlx::query("INSERT OR REPLACE INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, ?, ?, 0, 0, ?)")
                .bind(&state.account_id)
                .bind(&state.mailbox)
                .bind(&state.remote_name)
                .bind(uid_validity)
                .bind(Utc::now())
                .execute(&self.pool)
                .await?;
            sqlx::query("INSERT OR REPLACE INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid, uid_validity) VALUES (?, ?, ?, NULL, ?)")
                .bind(&state.account_id)
                .bind(&state.mailbox)
                .bind(Utc::now())
                .bind(uid_validity)
                .execute(&self.pool)
                .await?;
        }

        let messages: Vec<LegacyMessageRow> = sqlx::query_as(
            "SELECT m.id, m.accountId AS account_id, mb.path AS mailbox, m.uid, m.messageId AS message_id, m.inReplyTo AS in_reply_to, m.referencesJson AS reference_ids, m.threadId AS thread_id, m.subject, m.fromAddress AS from_address, m.toAddresses AS to_addresses, m.date, m.flags, m.snippet FROM legacy_messages m JOIN legacy_mailboxes mb ON mb.id = m.mailboxId",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut transaction = self.pool.begin().await?;
        for legacy in messages {
            let received_at = legacy_received_at(legacy.date.as_deref());
            let flags = legacy.flags.to_ascii_lowercase();
            sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?, NULL, ?, ?, '', '', '', ?, ?, '', NULL, 'headers_only', NULL, NULL, 0, ?, ?, 0, 0, 0, NULL, NULL, NULL, '')")
                .bind(&legacy.id)
                .bind(&legacy.account_id)
                .bind(&legacy.mailbox)
                .bind(legacy.uid)
                .bind(&legacy.message_id)
                .bind(&legacy.in_reply_to)
                .bind(&legacy.reference_ids)
                .bind(legacy.thread_id.unwrap_or_else(|| legacy.id.clone()))
                .bind(legacy.subject.unwrap_or_default())
                .bind(&legacy.from_address)
                .bind(&legacy.to_addresses)
                .bind(received_at)
                .bind(legacy.snippet.unwrap_or_default())
                .bind(flags.contains("\\\\seen"))
                .bind(flags.contains("\\\\flagged"))
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;

        for table in [
            "legacy_message_bodies",
            "legacy_thread_messages",
            "legacy_messages",
            "legacy_sync_state",
            "legacy_local_drafts",
            "legacy_operation_queue",
            "legacy_audit_log",
            "legacy_threads",
            "legacy_mailboxes",
            "legacy_accounts",
        ] {
            sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
                .execute(&self.pool)
                .await?;
        }
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('legacy_desktop_profile_migrated', '1') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn migrate_to_metadata_catalogue(&self) -> Result<()> {
        let version: Option<String> =
            sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'catalogue_schema'")
                .fetch_optional(&self.pool)
                .await?;
        if version.as_deref() == Some("1") {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        for statement in [
            "DROP TRIGGER IF EXISTS messages_ai",
            "DROP TRIGGER IF EXISTS messages_ad",
            "DROP TRIGGER IF EXISTS messages_au",
            "DROP TABLE IF EXISTS messages_fts",
            "UPDATE messages SET body_text = '', body_html = NULL",
            "DELETE FROM attachments",
            "CREATE VIRTUAL TABLE messages_fts USING fts5(subject, from_name, from_address, to_addresses, snippet, content='messages', content_rowid='rowid')",
            "CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, snippet) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.snippet); END",
            "CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, snippet) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.snippet); END",
            "CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, snippet) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.snippet); INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, snippet) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.snippet); END",
            "INSERT INTO messages_fts(messages_fts) VALUES ('rebuild')",
        ] {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("catalogue migration statement failed: {statement}"))?;
        }
        tx.commit().await?;
        // Deleting legacy body and attachment blobs only releases SQLite
        // pages internally. Compact once so the user's disk space is actually
        // returned; the catalogue_schema marker prevents repeated VACUUMs.
        sqlx::query("VACUUM").execute(&self.pool).await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('catalogue_schema', '1') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Seeds durable local mailbox identities from the compatibility state.
    /// The legacy table remains the source of existing sync watermarks while
    /// provider discovery progressively enriches these rows.
    async fn migrate_selectable_mailboxes(&self) -> Result<()> {
        sqlx::query(
            "INSERT INTO selectable_mailboxes(id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage, updated_at) \
             SELECT lower(hex(randomblob(16))), account_id, remote_name, mailbox, NULL, NULL, NULL, NULL, 1, uid_validity, CASE WHEN historical_complete = 1 THEN 'complete' ELSE 'partial' END, updated_at \
             FROM mailbox_catalog_state legacy \
             WHERE NOT EXISTS (SELECT 1 FROM selectable_mailboxes mailbox WHERE mailbox.account_id = legacy.account_id AND mailbox.remote_path = legacy.remote_name)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Converts the mailbox key used by durable local rows only where the
    /// selectable-mailbox catalogue proves the provider identity.  Earlier
    /// builds encoded a resolved special mailbox as `Sent::Foo`, which is
    /// ambiguous because a provider may also expose an ordinary mailbox with
    /// that exact literal name.  Do not infer from punctuation, case, or a
    /// display path: the exact raw remote path, special-use flag and, when
    /// present, UIDVALIDITY must agree before a row is moved.
    ///
    /// One transaction covers both the provider locator and every local
    /// dependent.  A crash therefore leaves either the old namespace or the
    /// complete new namespace, never a half-moved cache or source marker.
    async fn migrate_opaque_mailbox_storage_identities(&self) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT OR IGNORE INTO opaque_mailbox_storage_identity_progress(singleton) VALUES (1)",
        )
        .execute(&mut *tx)
        .await?;
        let (last_account_id, last_mailbox, complete): (String, String, bool) = sqlx::query_as(
            "SELECT last_account_id, last_mailbox, complete FROM opaque_mailbox_storage_identity_progress WHERE singleton = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        if complete {
            tx.commit().await?;
            return Ok(());
        }

        let account_data: Vec<(String, String)> = sqlx::query_as("SELECT id, data FROM accounts")
            .fetch_all(&mut *tx)
            .await?;
        let mut accounts = HashMap::new();
        for (id, data) in account_data {
            // A corrupt account record is already unusable for provider work.
            // Keep its legacy namespace untouched rather than making a
            // mailbox-identity decision from incomplete data.
            if let Ok(account) = deserialize_account(&data) {
                accounts.insert(id, account);
            }
        }

        let selectable: Vec<SelectableMailboxIdentityRow> = sqlx::query_as(
            "SELECT id, account_id, remote_path, local_path, hierarchy_delimiter, special_use, uid_validity FROM selectable_mailboxes",
        )
        .fetch_all(&mut *tx)
        .await?;
        let catalogue_states: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT account_id, mailbox, remote_name, uid_validity FROM mailbox_catalog_state",
        )
        .fetch_all(&mut *tx)
        .await?;
        let catalogue_by_locator = catalogue_states
            .iter()
            .map(|(account_id, mailbox, remote_name, uid_validity)| {
                (
                    (account_id.clone(), mailbox.clone()),
                    (remote_name.clone(), *uid_validity),
                )
            })
            .collect::<HashMap<_, _>>();

        // A mailbox can have no current message while still owning a sync
        // watermark, tombstone, or contacted-people source marker.  Include
        // every such durable namespace in the compatibility pass.
        let legacy_locators: Vec<(String, String)> = sqlx::query_as(
            "WITH locators AS ( \
               SELECT DISTINCT account_id, mailbox FROM messages \
               UNION SELECT DISTINCT account_id, mailbox FROM mailbox_catalog_state \
               UNION SELECT DISTINCT account_id, mailbox FROM mailbox_sync_state \
               UNION SELECT DISTINCT account_id, mailbox FROM mailbox_action_tombstones \
               UNION SELECT DISTINCT account_id, mailbox FROM contacted_people_backfill_sources \
             ) SELECT account_id, mailbox FROM locators \
             WHERE account_id > ? OR (account_id = ? AND mailbox > ?) \
             ORDER BY account_id, mailbox LIMIT ?",
        )
        .bind(&last_account_id)
        .bind(&last_account_id)
        .bind(&last_mailbox)
        .bind(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await?;
        let last_locator = legacy_locators.last().cloned();
        if last_locator.is_none() {
            sqlx::query("UPDATE opaque_mailbox_storage_identity_progress SET complete = 1 WHERE singleton = 1")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        }

        let mut migrations = Vec::new();
        for (account_id, legacy_mailbox) in legacy_locators {
            if is_opaque_mailbox_storage_identity(&legacy_mailbox) {
                continue;
            }
            let Some(account) = accounts.get(&account_id) else {
                continue;
            };
            let state = catalogue_by_locator.get(&(account_id.clone(), legacy_mailbox.clone()));
            let mut candidates = selectable
                .iter()
                .filter(|(_, candidate_account, ..)| candidate_account == &account_id)
                .filter_map(|candidate| {
                    opaque_mailbox_migration_target(
                        account,
                        &legacy_mailbox,
                        state.map(|(remote, uid_validity)| (remote.as_str(), *uid_validity)),
                        candidate,
                    )
                })
                .collect::<Vec<_>>();
            let mut targets = candidates
                .iter()
                .map(|(_, target)| target.clone())
                .collect::<HashSet<_>>();
            // More than one compatible target is an unresolved identity
            // collision.  Leave it alone so the next authoritative LIST and
            // catalogue rebuild can resolve it safely.
            if targets.len() != 1 {
                note_unresolved_opaque_mailbox_locator_in_tx(&mut tx, &account_id, &legacy_mailbox)
                    .await?;
                continue;
            }
            let (selectable_id, target) = candidates.remove(0);
            targets.clear();
            if target == legacy_mailbox {
                continue;
            }
            if !opaque_mailbox_target_is_safe_in_tx(&mut tx, &account_id, &legacy_mailbox, &target)
                .await?
            {
                note_unresolved_opaque_mailbox_locator_in_tx(&mut tx, &account_id, &legacy_mailbox)
                    .await?;
                continue;
            }
            migrations.push((account_id, legacy_mailbox, target, selectable_id));
        }

        let mut remaining_message_budget =
            usize::try_from(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE)
                .expect("positive migration batch size");
        let mut incomplete_locator = None;
        for (account_id, legacy_mailbox, target, selectable_id) in migrations {
            if remaining_message_budget == 0 {
                incomplete_locator = Some((account_id, legacy_mailbox));
                break;
            }
            // The catalogue record is the source of truth for the raw remote
            // identity.  Its local path becomes the new opaque storage key.
            sqlx::query(
                "UPDATE selectable_mailboxes SET local_path = ? WHERE id = ? AND account_id = ?",
            )
            .bind(&target)
            .bind(&selectable_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;

            migrate_mailbox_catalogue_state_in_tx(&mut tx, &account_id, &legacy_mailbox, &target)
                .await?;
            migrate_mailbox_sync_state_in_tx(&mut tx, &account_id, &legacy_mailbox, &target)
                .await?;
            sqlx::query(
                "INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) \
                 SELECT account_id, ?, uid, created_at FROM mailbox_action_tombstones \
                 WHERE account_id = ? AND mailbox = ? \
                 ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET \
                   created_at = CASE WHEN excluded.created_at > mailbox_action_tombstones.created_at THEN excluded.created_at ELSE mailbox_action_tombstones.created_at END",
            )
            .bind(&target)
            .bind(&account_id)
            .bind(&legacy_mailbox)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT OR IGNORE INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) \
                 SELECT account_id, ?, uid_validity, uid FROM contacted_people_backfill_sources \
                 WHERE account_id = ? AND mailbox = ?",
            )
            .bind(&target)
            .bind(&account_id)
            .bind(&legacy_mailbox)
            .execute(&mut *tx)
            .await?;

            let messages: Vec<(String, i64)> = sqlx::query_as(
                "SELECT id, uid FROM messages WHERE account_id = ? AND mailbox = ? ORDER BY id LIMIT ?",
            )
            .bind(&account_id)
            .bind(&legacy_mailbox)
            .bind(i64::try_from(remaining_message_budget).expect("migration batch fits i64"))
            .fetch_all(&mut *tx)
            .await?;
            remaining_message_budget = remaining_message_budget.saturating_sub(messages.len());
            let account_uuid = AccountId::parse_str(&account_id)
                .context("stored message has an invalid account identity")?;
            for (old_message_id, uid) in messages {
                let uid = u32::try_from(uid).context("stored message UID is outside IMAP range")?;
                let new_message_id = stable_message_id(account_uuid, &target, uid);
                let published_target_exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)",
                )
                .bind(&account_id)
                .bind(&target)
                .bind(i64::from(uid))
                .fetch_one(&mut *tx)
                .await?;
                if published_target_exists {
                    merge_legacy_message_into_published_opaque_row_in_tx(
                        &mut tx,
                        &account_id,
                        &old_message_id,
                        &new_message_id,
                    )
                    .await?;
                    continue;
                }
                rewrite_attachment_identity_in_tx(&mut tx, &old_message_id, &new_message_id)
                    .await?;
                sqlx::query(
                    "UPDATE messages SET id = ?, mailbox = ? WHERE id = ? AND account_id = ?",
                )
                .bind(&new_message_id)
                .bind(&target)
                .bind(&old_message_id)
                .bind(&account_id)
                .execute(&mut *tx)
                .await?;
                // Foreign-key cascades perform these updates in current
                // profiles.  Keep explicit fallbacks for historical SQLite
                // connections that opened before foreign keys were enabled.
                for table in [
                    "message_mailbox_memberships",
                    "attachments",
                    "message_attachment_catalogue",
                    "starred_message_bodies",
                    "starred_attachment_metadata",
                    "message_content_cache",
                    "message_search_body_text",
                    "message_content_fetches",
                ] {
                    sqlx::query(&format!(
                        "UPDATE {table} SET message_id = ? WHERE message_id = ?"
                    ))
                    .bind(&new_message_id)
                    .bind(&old_message_id)
                    .execute(&mut *tx)
                    .await?;
                }
                for table in [
                    "contacted_people_backfill_messages",
                    "contacted_people_legacy_unresolved_sources",
                ] {
                    sqlx::query(&format!(
                        "INSERT OR IGNORE INTO {table}(account_id, message_id) SELECT account_id, ? FROM {table} WHERE account_id = ? AND message_id = ?"
                    ))
                    .bind(&new_message_id)
                    .bind(&account_id)
                    .bind(&old_message_id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(&format!(
                        "DELETE FROM {table} WHERE account_id = ? AND message_id = ?"
                    ))
                    .bind(&account_id)
                    .bind(&old_message_id)
                    .execute(&mut *tx)
                    .await?;
                }
                rewrite_cached_attachment_identity_in_tx(&mut tx, &new_message_id, &old_message_id)
                    .await?;
            }
            let has_remaining_messages: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE account_id = ? AND mailbox = ?)",
            )
            .bind(&account_id)
            .bind(&legacy_mailbox)
            .fetch_one(&mut *tx)
            .await?;
            if has_remaining_messages {
                incomplete_locator = Some((account_id, legacy_mailbox));
                break;
            }
            // Keep the old locator's UIDVALIDITY and source markers through
            // every partial batch. The final batch is the only point at which
            // no old message can need that proof again.
            for table in [
                "mailbox_catalog_state",
                "mailbox_sync_state",
                "mailbox_action_tombstones",
                "contacted_people_backfill_sources",
            ] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE account_id = ? AND mailbox = ?"
                ))
                .bind(&account_id)
                .bind(&legacy_mailbox)
                .execute(&mut *tx)
                .await?;
            }
        }

        let (last_account_id, last_mailbox) = if incomplete_locator.is_some() {
            // Retry this source locator next turn. The keyset cursor remains
            // before it, while each committed transaction has already moved
            // at most the fixed message budget.
            (last_account_id, last_mailbox)
        } else {
            last_locator.expect("non-empty batch")
        };
        sqlx::query("UPDATE opaque_mailbox_storage_identity_progress SET last_account_id = ?, last_mailbox = ?, complete = 0 WHERE singleton = 1")
            .bind(last_account_id)
            .bind(last_mailbox)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Adds searchable catalogue structures without replacing the legacy FTS
    /// table. Keeping these side-by-side makes the migration restart-safe and
    /// avoids deleting cached message data merely to change an index shape.
    async fn migrate_search_catalogue_v2(&self) -> Result<()> {
        // `messages_fts_v2` started life as an external-content FTS table.
        // That shape is unsafe for a resumable build: before a legacy row has
        // reached its backfill batch, an UPDATE/DELETE trigger cannot issue an
        // FTS5 special delete for a row which has no index entry yet. Store the
        // indexed columns in this additive table instead. Normal FTS DELETE
        // and INSERT OR REPLACE are then idempotent whether a row was indexed
        // by a prior batch, by a live write, or by neither.
        let v2_definition: Option<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts_v2'",
        )
        .fetch_optional(&self.pool)
        .await?;
        let v2_uses_external_messages = v2_definition.as_deref().is_some_and(|definition| {
            definition
                .to_ascii_lowercase()
                .contains("content='messages'")
        });
        if v2_uses_external_messages {
            for statement in [
                "DROP TRIGGER IF EXISTS messages_fts_v2_ai",
                "DROP TRIGGER IF EXISTS messages_fts_v2_ad",
                "DROP TRIGGER IF EXISTS messages_fts_v2_au",
                "DROP TABLE messages_fts_v2",
                "DELETE FROM search_catalogue_v2_progress WHERE stage = 'headers'",
                "DELETE FROM app_meta WHERE key = 'search_catalogue_v2'",
            ] {
                sqlx::query(statement).execute(&self.pool).await?;
            }
        }
        // The initial development form used FTS5's contentless delete command
        // against an ordinary FTS table. Replace those two triggers on open so
        // an interrupted development build cannot leave body-cache writes
        // failing with a generic SQLite logic error.
        for statement in [
            // Recreate these on every open so a database interrupted between
            // the old and new trigger definitions is repaired before any live
            // catalogue write can run.
            "DROP TRIGGER IF EXISTS messages_fts_v2_ai",
            "DROP TRIGGER IF EXISTS messages_fts_v2_ad",
            "DROP TRIGGER IF EXISTS messages_fts_v2_au",
            "DROP TRIGGER IF EXISTS message_cached_bodies_fts_ad",
            "DROP TRIGGER IF EXISTS message_cached_bodies_fts_au",
            // Attachment cache eviction must not erase durable header-only
            // catalogue metadata written by persist_message.
            "DROP TRIGGER IF EXISTS message_attachment_catalogue_ad",
            "DROP TRIGGER IF EXISTS starred_attachment_catalogue_ad",
            // Recreate the insert triggers too: older databases have the
            // pre-presentation definition under the same trigger names.
            "DROP TRIGGER IF EXISTS message_attachment_catalogue_ai",
            "DROP TRIGGER IF EXISTS starred_attachment_catalogue_ai",
        ] {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        for statement in [
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_v2 USING fts5(subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet)",
            "CREATE TRIGGER IF NOT EXISTS messages_fts_v2_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts_v2(rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.cc_addresses, new.bcc_addresses, new.snippet); END",
            "CREATE TRIGGER IF NOT EXISTS messages_fts_v2_ad AFTER DELETE ON messages BEGIN DELETE FROM messages_fts_v2 WHERE rowid = old.rowid; END",
            "CREATE TRIGGER IF NOT EXISTS messages_fts_v2_au AFTER UPDATE ON messages BEGIN DELETE FROM messages_fts_v2 WHERE rowid = old.rowid; INSERT INTO messages_fts_v2(rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.cc_addresses, new.bcc_addresses, new.snippet); END",
            "CREATE VIRTUAL TABLE IF NOT EXISTS message_cached_bodies_fts USING fts5(message_id UNINDEXED, body_text)",
            "CREATE TRIGGER IF NOT EXISTS message_cached_bodies_fts_ai AFTER INSERT ON message_content_cache BEGIN INSERT INTO message_cached_bodies_fts(rowid, message_id, body_text) VALUES (new.rowid, new.message_id, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS message_cached_bodies_fts_ad AFTER DELETE ON message_content_cache BEGIN DELETE FROM message_cached_bodies_fts WHERE rowid = old.rowid; END",
            "CREATE TRIGGER IF NOT EXISTS message_cached_bodies_fts_au AFTER UPDATE ON message_content_cache BEGIN DELETE FROM message_cached_bodies_fts WHERE rowid = old.rowid; INSERT INTO message_cached_bodies_fts(rowid, message_id, body_text) VALUES (new.rowid, new.message_id, new.body_text); END",
            "CREATE VIRTUAL TABLE IF NOT EXISTS message_search_bodies_fts USING fts5(message_id UNINDEXED, body_text)",
            "CREATE TRIGGER IF NOT EXISTS message_search_bodies_fts_ai AFTER INSERT ON message_search_body_text BEGIN INSERT INTO message_search_bodies_fts(rowid, message_id, body_text) VALUES (new.rowid, new.message_id, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS message_search_bodies_fts_ad AFTER DELETE ON message_search_body_text BEGIN DELETE FROM message_search_bodies_fts WHERE rowid = old.rowid; END",
            "CREATE TRIGGER IF NOT EXISTS message_search_bodies_fts_au AFTER UPDATE ON message_search_body_text BEGIN DELETE FROM message_search_bodies_fts WHERE rowid = old.rowid; INSERT INTO message_search_bodies_fts(rowid, message_id, body_text) VALUES (new.rowid, new.message_id, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS message_attachment_catalogue_ai AFTER INSERT ON attachments BEGIN INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) VALUES (new.message_id, new.id, new.filename, new.mime_type, new.size_bytes, new.is_inline, new.presentation) ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation; END",
            "CREATE TRIGGER IF NOT EXISTS starred_attachment_catalogue_ai AFTER INSERT ON starred_attachment_metadata BEGIN INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) VALUES (new.message_id, new.id, new.filename, new.mime_type, new.size_bytes, new.is_inline, new.presentation) ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation; END",
        ] {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        let legacy_indexed: Option<String> =
            sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'search_catalogue_v2'")
                .fetch_optional(&self.pool)
                .await?;
        // Builds written before the resumable migration marker had already
        // completed their v2 header/cache/attachment rebuild synchronously.
        // Preserve that finished work rather than duplicating FTS rows.
        if legacy_indexed.as_deref() == Some("1") {
            for stage in [
                "headers",
                "cached_bodies",
                "attachments",
                "starred_attachments",
            ] {
                sqlx::query("INSERT OR IGNORE INTO search_catalogue_v2_progress(stage, complete) VALUES (?, 1)")
                    .bind(stage)
                    .execute(&self.pool)
                    .await?;
            }
        } else {
            self.migrate_search_catalogue_v2_stage(
                "headers",
                "messages",
                "INSERT OR REPLACE INTO messages_fts_v2(rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet) SELECT rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet FROM messages WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
            )
            .await?;
            self.migrate_search_catalogue_v2_stage(
                "cached_bodies",
                "message_content_cache",
                "INSERT OR REPLACE INTO message_cached_bodies_fts(rowid, message_id, body_text) SELECT rowid, message_id, body_text FROM message_content_cache WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
            )
            .await?;
            self.migrate_search_catalogue_v2_stage(
                "attachments",
                "attachments",
                "INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT message_id, id, filename, mime_type, size_bytes, is_inline, presentation FROM attachments WHERE rowid > ? AND rowid <= ? ORDER BY rowid ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation",
            )
            .await?;
            self.migrate_search_catalogue_v2_stage(
                "starred_attachments",
                "starred_attachment_metadata",
                "INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT message_id, id, filename, mime_type, size_bytes, is_inline, presentation FROM starred_attachment_metadata WHERE rowid > ? AND rowid <= ? ORDER BY rowid ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation",
            )
            .await?;
        }
        self.migrate_search_catalogue_v2_stage(
            "search_bodies",
            "message_search_body_text",
            "INSERT OR REPLACE INTO message_search_bodies_fts(rowid, message_id, body_text) SELECT rowid, message_id, body_text FROM message_search_body_text WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
        )
        .await?;
        if self.search_catalogue_v2_complete().await? {
            sqlx::query("INSERT INTO app_meta(key, value) VALUES ('search_catalogue_v2', '1') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Moves one committed, fixed-size source slice into a v2 search index.
    /// Keeping the cursor and insert in one transaction makes an interrupted
    /// open restart from the last published batch instead of rebuilding all
    /// 50,000-plus rows in one foreground startup.
    async fn migrate_search_catalogue_v2_stage(
        &self,
        stage: &str,
        source_table: &str,
        insert_sql: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO search_catalogue_v2_progress(stage) VALUES (?)")
            .bind(stage)
            .execute(&mut *tx)
            .await?;
        let (last_rowid, mut target_rowid, complete): (i64, i64, bool) = sqlx::query_as(
            "SELECT last_rowid, target_rowid, complete FROM search_catalogue_v2_progress WHERE stage = ?",
        )
        .bind(stage)
        .fetch_one(&mut *tx)
        .await?;
        if complete {
            tx.commit().await?;
            return Ok(());
        }
        if target_rowid == 0 {
            target_rowid = sqlx::query_scalar(&format!(
                "SELECT COALESCE(MAX(rowid), 0) FROM {source_table}"
            ))
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE search_catalogue_v2_progress SET target_rowid = ? WHERE stage = ?")
                .bind(target_rowid)
                .bind(stage)
                .execute(&mut *tx)
                .await?;
        }
        let batch_rowids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT rowid FROM {source_table} WHERE rowid > ? AND rowid <= ? ORDER BY rowid LIMIT ?"
        ))
        .bind(last_rowid)
        .bind(target_rowid)
        .bind(SEARCH_CATALOGUE_V2_MIGRATION_BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await?;
        let Some(next_rowid) = batch_rowids.last().copied() else {
            sqlx::query("UPDATE search_catalogue_v2_progress SET complete = 1 WHERE stage = ?")
                .bind(stage)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        };
        sqlx::query(insert_sql)
            .bind(last_rowid)
            .bind(next_rowid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE search_catalogue_v2_progress SET last_rowid = ? WHERE stage = ?")
            .bind(next_rowid)
            .bind(stage)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn search_catalogue_v2_complete(&self) -> Result<bool> {
        let incomplete: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_catalogue_v2_progress WHERE stage IN ('headers', 'cached_bodies', 'attachments', 'starred_attachments', 'search_bodies') AND complete = 0",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(incomplete == 0)
    }

    /// Advances each independent v2 catalogue source by at most one fixed
    /// batch. Call this from a yielding background task after startup; it is
    /// intentionally safe to call again after interruption or restart.
    pub async fn advance_search_catalogue_v2_backfill(
        &self,
    ) -> Result<SearchCatalogueV2BackfillProgress> {
        // Share the yielding low-priority maintenance cadence with the search
        // catalogue. Each call converts at most one opaque-locator batch, so
        // a profile upgrade cannot monopolize startup or message opening.
        self.migrate_opaque_mailbox_storage_identities().await?;
        self.migrate_search_catalogue_v2_stage(
            "headers",
            "messages",
            "INSERT OR REPLACE INTO messages_fts_v2(rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet) SELECT rowid, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, snippet FROM messages WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
        )
        .await?;
        self.migrate_search_catalogue_v2_stage(
            "cached_bodies",
            "message_content_cache",
            "INSERT OR REPLACE INTO message_cached_bodies_fts(rowid, message_id, body_text) SELECT rowid, message_id, body_text FROM message_content_cache WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
        )
        .await?;
        self.migrate_search_catalogue_v2_stage(
            "attachments",
            "attachments",
            "INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT message_id, id, filename, mime_type, size_bytes, is_inline, presentation FROM attachments WHERE rowid > ? AND rowid <= ? ORDER BY rowid ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation",
        )
        .await?;
        self.migrate_search_catalogue_v2_stage(
            "starred_attachments",
            "starred_attachment_metadata",
            "INSERT INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT message_id, id, filename, mime_type, size_bytes, is_inline, presentation FROM starred_attachment_metadata WHERE rowid > ? AND rowid <= ? ORDER BY rowid ON CONFLICT(message_id, attachment_id) DO UPDATE SET filename = excluded.filename, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, is_inline = excluded.is_inline, presentation = excluded.presentation",
        )
        .await?;
        self.migrate_search_catalogue_v2_stage(
            "search_bodies",
            "message_search_body_text",
            "INSERT OR REPLACE INTO message_search_bodies_fts(rowid, message_id, body_text) SELECT rowid, message_id, body_text FROM message_search_body_text WHERE rowid > ? AND rowid <= ? ORDER BY rowid",
        )
        .await?;
        if self.search_catalogue_v2_complete().await? {
            sqlx::query("INSERT INTO app_meta(key, value) VALUES ('search_catalogue_v2', '1') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .execute(&self.pool)
                .await?;
        }
        self.search_catalogue_v2_backfill_progress().await
    }

    pub async fn search_catalogue_v2_backfill_progress(
        &self,
    ) -> Result<SearchCatalogueV2BackfillProgress> {
        let header: Option<(i64, i64, bool)> = sqlx::query_as(
            "SELECT last_rowid, target_rowid, complete FROM search_catalogue_v2_progress WHERE stage = 'headers'",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some((indexed_messages, total_messages, complete)) = header else {
            return Ok(SearchCatalogueV2BackfillProgress {
                indexed_messages: 0,
                total_messages: 0,
                complete: false,
            });
        };
        let opaque_locator_complete: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT complete FROM opaque_mailbox_storage_identity_progress WHERE singleton = 1), 0)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(SearchCatalogueV2BackfillProgress {
            indexed_messages,
            total_messages,
            // The desktop background loop uses this flag as its stop signal.
            // Keep it false until both maintenance streams finish, otherwise
            // an already-built FTS catalogue would strand locator rows after
            // the first 500-message compatibility batch.
            complete: complete
                && self.search_catalogue_v2_complete().await?
                && opaque_locator_complete,
        })
    }

    /// Counts body text available from a search-only response, a complete
    /// foreground reader cache, or the durable starred cache. All lookups are
    /// keyed by message ID and the outer message scan is account-indexed.
    pub async fn local_body_index_coverage(
        &self,
        account_id: AccountId,
    ) -> Result<LocalBodyIndexCoverage> {
        let account_key = account_id.to_string();
        let (catalogue_messages, searchable_bodies): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN EXISTS (SELECT 1 FROM message_search_body_text s WHERE s.message_id = m.id) OR EXISTS (SELECT 1 FROM message_content_cache c WHERE c.message_id = m.id AND c.content_state = 'complete') OR EXISTS (SELECT 1 FROM starred_message_bodies b WHERE b.message_id = m.id AND b.attachment_presentation_version = ?) THEN 1 ELSE 0 END), 0) FROM messages m WHERE m.account_id = ?",
        )
        .bind(ATTACHMENT_PRESENTATION_VERSION)
        .bind(&account_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(LocalBodyIndexCoverage {
            account_id: account_key,
            catalogue_messages,
            searchable_bodies,
        })
    }

    pub async fn set_secret(&self, name: &str, secret: &str) -> Result<()> {
        let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
        let ciphertext = encrypt_secret(&self.vault_key, nonce, name, secret)?;
        sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(name) DO UPDATE SET nonce=excluded.nonce, ciphertext=excluded.ciphertext, updated_at=excluded.updated_at")
            .bind(name)
            .bind(nonce.as_slice())
            .bind(ciphertext)
            .bind(Utc::now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn secret(&self, name: &str) -> Result<Option<String>> {
        let row: Option<(Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT nonce, ciphertext FROM credentials WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(nonce, ciphertext)| decrypt_secret(&self.vault_key, &nonce, name, ciphertext))
            .transpose()
    }

    pub async fn delete_secret(&self, name: &str) -> Result<()> {
        sqlx::query("DELETE FROM credentials WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_account(&self, account: &Account) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let active_membership_changed = save_account_in_transaction(&mut tx, account).await?;
        tx.commit().await?;
        if active_membership_changed {
            // Small histories converge immediately; large histories advance
            // one bounded writer batch and the existing background worker
            // resumes through `continue_contacted_people_migrations`.
            self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await?;
        }
        Ok(())
    }

    /// Persists an account and its reset-required rebuild job as one commit.
    /// This is used when a provider identity changes: callers cannot publish
    /// the new account data before durable replacement intent exists.
    pub async fn save_account_with_reset_mail_rebuild_job(
        &self,
        account: &Account,
        job: &MailRebuildJob,
    ) -> Result<()> {
        if job.account_id != account.id {
            return Err(anyhow!("mail rebuild job belongs to a different account"));
        }
        if !job.reset_before_sync {
            return Err(anyhow!(
                "identity replacement requires a reset-before-sync rebuild job"
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active_membership_changed = save_account_in_transaction(&mut tx, account).await?;
        save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        tx.commit().await?;
        if active_membership_changed {
            self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await?;
        }
        Ok(())
    }

    /// Commits a changed account identity, durable reset intent, and removal
    /// of its superseded credential key together. The replacement credential
    /// is deliberately written before this boundary, under
    /// `current_secret_name`; rejecting equal names makes it impossible for
    /// this cleanup to erase that newly written key.
    pub async fn save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
        &self,
        account: &Account,
        job: &MailRebuildJob,
        previous_secret_name: Option<&str>,
        current_secret_name: &str,
    ) -> Result<()> {
        if job.account_id != account.id {
            return Err(anyhow!("mail rebuild job belongs to a different account"));
        }
        if !job.reset_before_sync {
            return Err(anyhow!(
                "identity replacement requires a reset-before-sync rebuild job"
            ));
        }
        if previous_secret_name.is_some_and(|name| name == current_secret_name) {
            return Err(anyhow!(
                "previous credential key must differ from the current credential key"
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active_membership_changed = save_account_in_transaction(&mut tx, account).await?;
        save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        if let Some(previous_secret_name) = previous_secret_name {
            sqlx::query("DELETE FROM credentials WHERE name = ?")
                .bind(previous_secret_name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        if active_membership_changed {
            self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await?;
        }
        Ok(())
    }

    /// Replaces an account and its encrypted credential in one SQLite commit.
    /// This keeps persisted auth metadata and credential contents consistent
    /// across crashes while an account changes authentication schemes.
    pub async fn save_account_with_secret(
        &self,
        account: &Account,
        secret_name: &str,
        secret: &str,
    ) -> Result<()> {
        let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
        let ciphertext = encrypt_secret(&self.vault_key, nonce, secret_name, secret)?;
        let mut tx = self.pool.begin().await?;
        let active_membership_changed = save_account_in_transaction(&mut tx, account).await?;
        sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(name) DO UPDATE SET nonce=excluded.nonce, ciphertext=excluded.ciphertext, updated_at=excluded.updated_at")
            .bind(secret_name)
            .bind(nonce.as_slice())
            .bind(ciphertext)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if active_membership_changed {
            self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await?;
        }
        Ok(())
    }

    /// Stores refreshed OAuth credentials only while the durable account still
    /// uses OAuth. A stale watcher cannot overwrite a newly saved app password.
    pub async fn set_oauth_secret_if_current(
        &self,
        account_id: AccountId,
        secret_name: &str,
        secret: &str,
    ) -> Result<bool> {
        let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
        let ciphertext = encrypt_secret(&self.vault_key, nonce, secret_name, secret)?;
        let mut tx = self.pool.begin().await?;
        let data: Option<String> = sqlx::query_scalar("SELECT data FROM accounts WHERE id = ?")
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let still_oauth = data
            .as_deref()
            .map(deserialize_account)
            .transpose()?
            .is_some_and(|account| matches!(account.auth, AccountAuth::OAuth2 { .. }));
        if !still_oauth {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(name) DO UPDATE SET nonce=excluded.nonce, ciphertext=excluded.ciphertext, updated_at=excluded.updated_at")
            .bind(secret_name)
            .bind(nonce.as_slice())
            .bind(ciphertext)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn accounts(&self) -> Result<Vec<Account>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT data FROM accounts ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|(data,)| deserialize_account(&data))
            .collect()
    }

    pub async fn account(&self, id: AccountId) -> Result<Option<Account>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM accounts WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(data,)| deserialize_account(&data)).transpose()
    }

    pub async fn mail_rebuild_jobs(&self) -> Result<Vec<MailRebuildJob>> {
        let rows: Vec<(String, String, i64, Option<i64>, bool)> = sqlx::query_as(
            "SELECT account_id, phase, completed, total, reset_before_sync FROM mail_rebuild_jobs ORDER BY updated_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(account_id, phase, completed, total, reset_before_sync)| {
                Ok(MailRebuildJob {
                    account_id: AccountId::parse_str(&account_id)?,
                    phase,
                    completed: usize::try_from(completed)?,
                    total: total.map(usize::try_from).transpose()?,
                    reset_before_sync,
                })
            })
            .collect()
    }

    pub async fn save_mail_rebuild_job(&self, job: &MailRebuildJob) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_mail_rebuild_job(&self, account_id: AccountId) -> Result<()> {
        sqlx::query("DELETE FROM mail_rebuild_jobs WHERE account_id = ?")
            .bind(account_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_account(&self, id: AccountId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let account_id = id.to_string();
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) SELECT ?, 0 WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?)")
            .bind(&account_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE account_search_generations SET generation = generation + 1 WHERE account_id = ?")
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO deleted_account_tombstones(account_id, deleted_at) VALUES (?, ?) ON CONFLICT(account_id) DO UPDATE SET deleted_at=excluded.deleted_at")
            .bind(&account_id)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mail_rebuild_jobs WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_catalog_state WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_snapshot_generations WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_sync_failures WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_sync_state WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sent_correspondents WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        remove_contacted_people_account_contribution_in_tx(&mut tx, &id.to_string()).await?;
        sqlx::query("DELETE FROM app_meta WHERE key IN (?, ?)")
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY)
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM selectable_mailboxes WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM messages WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
            .await?;
        Ok(())
    }

    /// Returns the current account generation used to reject stale provider
    /// search publications. The row is created only for a live account.
    pub async fn account_search_generation(&self, id: AccountId) -> Result<i64> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) SELECT ?, 0 WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?)")
            .bind(&account_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;
        let generation = sqlx::query_scalar(
            "SELECT generation FROM account_search_generations WHERE account_id = ?",
        )
        .bind(&account_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("account does not exist"))?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Invalidates provider-search publications for one account. Call this in
    /// the same foreground mutation path before configuration, rebuild, or
    /// local flag state changes become authoritative.
    pub async fn advance_account_search_generation(&self, id: AccountId) -> Result<i64> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) SELECT ?, 0 WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?)")
            .bind(&account_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;
        let updated = sqlx::query(
            "UPDATE account_search_generations SET generation = generation + 1 WHERE account_id = ?",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            tx.rollback().await?;
            return Err(anyhow!("account does not exist"));
        }
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM account_search_generations WHERE account_id = ?",
        )
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Creates or refreshes a provider-discovered mailbox without changing the
    /// legacy mailbox-sync row. Repeating the same discovery preserves the
    /// opaque local ID and atomically updates its metadata.
    pub async fn upsert_selectable_mailbox(
        &self,
        account_id: AccountId,
        draft: &SelectableMailboxDraft,
    ) -> Result<SelectableMailbox> {
        self.upsert_selectable_mailbox_with_generation(account_id, draft, None)
            .await?
            .ok_or_else(|| anyhow!("account does not exist"))
    }

    /// Generation-bound provider-search counterpart. `None` means a
    /// foreground mutation made this discovery stale, so no mailbox metadata
    /// is published.
    pub async fn upsert_selectable_mailbox_if_account_generation(
        &self,
        account_id: AccountId,
        generation: i64,
        draft: &SelectableMailboxDraft,
    ) -> Result<Option<SelectableMailbox>> {
        self.upsert_selectable_mailbox_with_generation(account_id, draft, Some(generation))
            .await
    }

    async fn upsert_selectable_mailbox_with_generation(
        &self,
        account_id: AccountId,
        draft: &SelectableMailboxDraft,
        expected_generation: Option<i64>,
    ) -> Result<Option<SelectableMailbox>> {
        // The provider's raw mailbox path is a wire identity.  Reject an
        // all-whitespace value, but never trim a valid name: ` Foo` and
        // `Foo` are distinct IMAP mailboxes and must retain distinct opaque
        // storage namespaces.
        let remote_path = draft.remote_path.as_str();
        if remote_path.trim().is_empty() {
            return Err(anyhow!("selectable mailbox remote path cannot be empty"));
        }
        if !matches!(
            draft.catalogue_coverage.as_str(),
            "unknown" | "partial" | "complete"
        ) {
            return Err(anyhow!("invalid mailbox catalogue coverage"));
        }
        let account_key = account_id.to_string();
        let local_path = draft
            .local_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(remote_path);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(generation) = expected_generation {
            if !account_search_generation_matches_in_tx(&mut tx, &account_key, generation).await? {
                tx.rollback().await?;
                return Ok(None);
            }
        }
        if let Some(parent_id) = draft.parent_id.as_deref() {
            let parent_account: Option<String> =
                sqlx::query_scalar("SELECT account_id FROM selectable_mailboxes WHERE id = ?")
                    .bind(parent_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if parent_account.as_deref() != Some(account_key.as_str()) {
                return Err(anyhow!("mailbox parent does not belong to this account"));
            }
        }
        sqlx::query("INSERT INTO selectable_mailboxes(id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, remote_path) DO UPDATE SET local_path=excluded.local_path, hierarchy_delimiter=excluded.hierarchy_delimiter, parent_id=excluded.parent_id, parent_path=excluded.parent_path, special_use=excluded.special_use, selectable=excluded.selectable, uid_validity=excluded.uid_validity, catalogue_coverage=excluded.catalogue_coverage, updated_at=excluded.updated_at")
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&account_key)
            .bind(remote_path)
            .bind(local_path)
            .bind(&draft.hierarchy_delimiter)
            .bind(&draft.parent_id)
            .bind(&draft.parent_path)
            .bind(&draft.special_use)
            .bind(draft.selectable)
            .bind(draft.uid_validity)
            .bind(&draft.catalogue_coverage)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        let mailbox = sqlx::query_as::<_, SelectableMailbox>("SELECT id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage FROM selectable_mailboxes WHERE account_id = ? AND remote_path = ?")
            .bind(&account_key)
            .bind(remote_path)
            .fetch_one(&mut *tx)
            .await?;
        let has_unresolved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM opaque_mailbox_storage_identity_unresolved WHERE account_id = ?)",
        )
        .bind(&account_key)
        .fetch_one(&mut *tx)
        .await?;
        if has_unresolved {
            // A new LIST/SELECT result may be the first authoritative raw
            // path, special-use flag, or UIDVALIDITY for an old locator.
            // Retry it on the next bounded maintenance turn, but do not
            // rewind completed profiles with no unresolved rows.
            sqlx::query(
                "DELETE FROM opaque_mailbox_storage_identity_unresolved WHERE account_id = ?",
            )
            .bind(&account_key)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE opaque_mailbox_storage_identity_progress SET last_account_id = '', last_mailbox = '', complete = 0 WHERE singleton = 1")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Some(mailbox))
    }

    pub async fn selectable_mailbox_catalogue(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<SelectableMailbox>> {
        Ok(sqlx::query_as::<_, SelectableMailbox>("SELECT id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage FROM selectable_mailboxes WHERE account_id = ? ORDER BY local_path, remote_path, id")
            .bind(account_id.to_string())
            .fetch_all(&self.pool)
            .await?)
    }

    /// Alias kept short for callers that only need the provider mailbox list.
    pub async fn list_selectable_mailboxes(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<SelectableMailbox>> {
        self.selectable_mailbox_catalogue(account_id).await
    }

    pub async fn delete_selectable_mailbox(
        &self,
        account_id: AccountId,
        mailbox_id: &str,
    ) -> Result<bool> {
        let deleted =
            sqlx::query("DELETE FROM selectable_mailboxes WHERE id = ? AND account_id = ?")
                .bind(mailbox_id)
                .bind(account_id.to_string())
                .execute(&self.pool)
                .await?
                .rows_affected();
        Ok(deleted == 1)
    }

    /// Applies a successful, complete provider LIST as the authoritative
    /// mailbox namespace for one account. Rows absent from the response are
    /// retired together with their cached catalogue namespace and logical
    /// memberships. A failed or malformed LIST must never call this method.
    pub async fn retire_selectable_mailboxes_absent_from_authoritative_list(
        &self,
        account_id: AccountId,
        listed_remote_paths: &HashSet<String>,
    ) -> Result<usize> {
        let account_key = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query_as::<_, SelectableMailbox>(
            "SELECT id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage FROM selectable_mailboxes WHERE account_id = ?",
        )
        .bind(&account_key)
        .fetch_all(&mut *tx)
        .await?;
        let stale = existing
            .into_iter()
            .filter(|mailbox| !listed_remote_paths.contains(&mailbox.remote_path))
            .collect::<Vec<_>>();
        if stale.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        // Current normal sync stores a stable opaque mailbox locator, while
        // older profiles may still carry local or remote-path locators. Clear
        // every unambiguous representation before the foreign-key cascade
        // removes memberships, so stale folders cannot remain locally found.
        let mut locators = HashSet::new();
        for mailbox in &stale {
            locators.insert(mailbox.remote_path.clone());
            locators.insert(mailbox.local_path.clone());
            locators.insert(selectable_mailbox_storage_locator(mailbox));
        }
        let remote_paths = stale
            .iter()
            .map(|mailbox| mailbox.remote_path.as_str())
            .collect::<Vec<_>>();
        let remote_placeholders = vec!["?"; remote_paths.len()].join(",");
        let state_sql = format!(
            "SELECT mailbox FROM mailbox_catalog_state WHERE account_id = ? AND remote_name IN ({remote_placeholders})"
        );
        let mut state_query = sqlx::query_scalar::<_, String>(&state_sql).bind(&account_key);
        for remote_path in &remote_paths {
            state_query = state_query.bind(remote_path);
        }
        locators.extend(state_query.fetch_all(&mut *tx).await?);

        if !locators.is_empty() {
            let locators = locators.into_iter().collect::<Vec<_>>();
            let placeholders = vec!["?"; locators.len()].join(",");
            let delete_messages = format!(
                "DELETE FROM messages WHERE account_id = ? AND mailbox IN ({placeholders})"
            );
            let mut delete_query = sqlx::query(&delete_messages).bind(&account_key);
            for locator in &locators {
                delete_query = delete_query.bind(locator);
            }
            delete_query.execute(&mut *tx).await?;
        }

        let delete_state = format!(
            "DELETE FROM mailbox_catalog_state WHERE account_id = ? AND remote_name IN ({remote_placeholders})"
        );
        let mut delete_state_query = sqlx::query(&delete_state).bind(&account_key);
        for remote_path in &remote_paths {
            delete_state_query = delete_state_query.bind(remote_path);
        }
        delete_state_query.execute(&mut *tx).await?;

        let stale_ids = stale
            .iter()
            .map(|mailbox| mailbox.id.as_str())
            .collect::<Vec<_>>();
        let id_placeholders = vec!["?"; stale_ids.len()].join(",");
        let delete_mailboxes = format!(
            "DELETE FROM selectable_mailboxes WHERE account_id = ? AND id IN ({id_placeholders})"
        );
        let mut delete_mailboxes_query = sqlx::query(&delete_mailboxes).bind(&account_key);
        for id in &stale_ids {
            delete_mailboxes_query = delete_mailboxes_query.bind(id);
        }
        delete_mailboxes_query.execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(stale.len())
    }

    /// Replaces the discovered mailbox members for one locally stable message
    /// ID. The transaction rejects a cross-account mailbox before deleting an
    /// existing membership, so an invalid provider update cannot widen scope.
    pub async fn set_message_mailbox_memberships(
        &self,
        account_id: AccountId,
        message_id: &str,
        mailbox_ids: &[String],
    ) -> Result<()> {
        self.set_message_mailbox_memberships_with_generation(
            account_id,
            message_id,
            mailbox_ids,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Generation-bound provider-search counterpart. It returns false rather
    /// than replacing logical mailbox membership after a concurrent account
    /// reset, reconfiguration, disable, or flag mutation.
    pub async fn set_message_mailbox_memberships_if_account_generation(
        &self,
        account_id: AccountId,
        generation: i64,
        message_id: &str,
        mailbox_ids: &[String],
    ) -> Result<bool> {
        self.set_message_mailbox_memberships_with_generation(
            account_id,
            message_id,
            mailbox_ids,
            Some(generation),
        )
        .await
    }

    async fn set_message_mailbox_memberships_with_generation(
        &self,
        account_id: AccountId,
        message_id: &str,
        mailbox_ids: &[String],
        expected_generation: Option<i64>,
    ) -> Result<bool> {
        let account_key = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(generation) = expected_generation {
            if !account_search_generation_matches_in_tx(&mut tx, &account_key, generation).await? {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        let message_account: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if message_account.as_deref() != Some(account_key.as_str()) {
            return Err(anyhow!("message does not belong to this account"));
        }
        let unique_mailbox_ids = mailbox_ids.iter().collect::<HashSet<_>>();
        for mailbox_id in &unique_mailbox_ids {
            let mailbox_account: Option<String> =
                sqlx::query_scalar("SELECT account_id FROM selectable_mailboxes WHERE id = ?")
                    .bind(mailbox_id.as_str())
                    .fetch_optional(&mut *tx)
                    .await?;
            if mailbox_account.as_deref() != Some(account_key.as_str()) {
                return Err(anyhow!("mailbox does not belong to this account"));
            }
        }
        sqlx::query(
            "DELETE FROM message_mailbox_memberships WHERE message_id = ? AND account_id = ?",
        )
        .bind(message_id)
        .bind(&account_key)
        .execute(&mut *tx)
        .await?;
        for mailbox_id in unique_mailbox_ids {
            sqlx::query("INSERT INTO message_mailbox_memberships(message_id, mailbox_id, account_id) VALUES (?, ?, ?)")
                .bind(message_id)
                .bind(mailbox_id)
                .bind(&account_key)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn list_message_mailbox_memberships(
        &self,
        account_id: AccountId,
        message_id: &str,
    ) -> Result<Vec<MessageMailboxMembership>> {
        Ok(sqlx::query_as::<_, MessageMailboxMembership>("SELECT message_id, mailbox_id, account_id FROM message_mailbox_memberships WHERE account_id = ? AND message_id = ? ORDER BY mailbox_id")
            .bind(account_id.to_string())
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Binds every durable message in one catalogue namespace to the exact
    /// selectable mailbox that produced it. This is used after a complete
    /// snapshot so replacement imports and interrupted older syncs are healed
    /// with one transaction instead of one transaction per message.
    pub async fn bind_catalog_mailbox_memberships(
        &self,
        account_id: AccountId,
        storage_mailbox: &str,
        selectable_mailbox_id: &str,
    ) -> Result<()> {
        let account_key = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mailbox_account: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM selectable_mailboxes WHERE id = ?")
                .bind(selectable_mailbox_id)
                .fetch_optional(&mut *tx)
                .await?;
        if mailbox_account.as_deref() != Some(account_key.as_str()) {
            tx.rollback().await?;
            return Err(anyhow!("mailbox does not belong to this account"));
        }
        sqlx::query("DELETE FROM message_mailbox_memberships WHERE account_id = ? AND message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)")
            .bind(&account_key)
            .bind(&account_key)
            .bind(storage_mailbox)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO message_mailbox_memberships(message_id, mailbox_id, account_id) SELECT id, ?, account_id FROM messages WHERE account_id = ? AND mailbox = ?")
            .bind(selectable_mailbox_id)
            .bind(&account_key)
            .bind(storage_mailbox)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records the unique final recipients of one SMTP-accepted outgoing
    /// message. The caller supplies every configured account address that
    /// must be excluded; addresses are never learned from draft text or from
    /// inbound mail through this API.
    pub async fn record_successful_outgoing_recipients(
        &self,
        account_id: AccountId,
        recipients: &[ContactedPersonRecipient],
        excluded_addresses: &[String],
    ) -> Result<usize> {
        let sequence = self.reserve_contacted_people_action_sequence().await?;
        self.record_successful_outgoing_recipients_at_sequence(
            account_id,
            recipients,
            excluded_addresses,
            sequence,
        )
        .await
    }

    pub async fn record_successful_outgoing_recipients_at_sequence(
        &self,
        account_id: AccountId,
        recipients: &[ContactedPersonRecipient],
        excluded_addresses: &[String],
        accepted_sequence: i64,
    ) -> Result<usize> {
        // Capture acceptance order before waiting for the SQLite writer. A
        // later Clear/Hide must win even when this accepted-send task obtains
        // the write lock afterwards.
        let accepted_at = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_live_contacted_people_account(&mut tx, &account_id.to_string()).await?;
        if !autocomplete_suggestions_enabled_in_tx(&mut tx).await? {
            tx.commit().await?;
            return Ok(0);
        }
        if contacted_people_clear_sequence_in_tx(&mut tx).await? >= accepted_sequence {
            tx.commit().await?;
            return Ok(0);
        }
        let recorded = record_contacted_people_in_tx(
            &mut tx,
            &account_id.to_string(),
            recipients,
            excluded_addresses,
            accepted_at,
            true,
            Some(accepted_sequence),
        )
        .await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// Records an SMTP-accepted message with its generated RFC Message-ID.
    /// The account-scoped source marker and recipient statistics commit
    /// together, so a later provider Sent copy cannot count the same send a
    /// second time.
    pub async fn record_successful_outgoing_recipients_with_message_id(
        &self,
        account_id: AccountId,
        rfc_message_id: &str,
        recipients: &[ContactedPersonRecipient],
        excluded_addresses: &[String],
    ) -> Result<usize> {
        let sequence = self.reserve_contacted_people_action_sequence().await?;
        self.record_successful_outgoing_recipients_with_message_id_at_sequence(
            account_id,
            rfc_message_id,
            recipients,
            excluded_addresses,
            sequence,
        )
        .await
    }

    pub async fn record_successful_outgoing_recipients_with_message_id_at_sequence(
        &self,
        account_id: AccountId,
        rfc_message_id: &str,
        recipients: &[ContactedPersonRecipient],
        excluded_addresses: &[String],
        accepted_sequence: i64,
    ) -> Result<usize> {
        let rfc_message_id = normalize_message_id(rfc_message_id)
            .ok_or_else(|| anyhow!("outgoing RFC Message-ID is invalid"))?;
        let account_id = account_id.to_string();
        let accepted_at = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_live_contacted_people_account(&mut tx, &account_id).await?;
        // Keep this SMTP acceptance marker even if a later privacy action or
        // disabled collection makes the recipient-stat portion a no-op. A
        // provider Sent copy can arrive after the next trusted cutoff, so it
        // must still be recognised as this already accepted message.
        let inserted = sqlx::query("INSERT OR IGNORE INTO contacted_people_outgoing_messages(account_id, rfc_message_id, recorded_at) VALUES (?, ?, ?)")
            .bind(&account_id)
            .bind(&rfc_message_id)
            .bind(accepted_at)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if !autocomplete_suggestions_enabled_in_tx(&mut tx).await? {
            tx.commit().await?;
            return Ok(0);
        }
        if contacted_people_clear_sequence_in_tx(&mut tx).await? >= accepted_sequence {
            tx.commit().await?;
            return Ok(0);
        }
        if !inserted {
            tx.commit().await?;
            return Ok(0);
        }
        let recorded = record_contacted_people_in_tx(
            &mut tx,
            &account_id,
            recipients,
            excluded_addresses,
            accepted_at,
            true,
            Some(accepted_sequence),
        )
        .await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// Returns at most eight local, non-hidden contacted people. Ranking is
    /// deterministic and intentionally happens after retrieval so Unicode
    /// display names and address parts receive the same comparison rules as
    /// the compose UI.
    pub async fn suggest_contacted_people(
        &self,
        query: &str,
        preferred_account_id: Option<AccountId>,
    ) -> Result<Vec<ContactedPersonSuggestion>> {
        let preferred_account_id = preferred_account_id.map(|id| id.to_string());
        // SQLite's built-in lower() is ASCII-only. Canonicalise configured
        // owner addresses in Rust as a final guard so an address learned
        // before a Unicode account address is configured is never suggested.
        let self_addresses: HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT email FROM accounts")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .filter_map(|address| canonical_contacted_address(&address))
                .collect();
        let query = normalize_contacted_people_match(query);
        if query.is_empty() {
            // Keep focus-without-typing bounded in SQLite. The preferred CTE
            // uses the account-recency index, and the fallback uses the
            // visible global-rank index. This is the same deterministic
            // ordering as the general matcher, without loading every local
            // person into Rust merely to show eight recent suggestions.
            let suggestions: Vec<ContactedPersonSuggestion> = sqlx::query_as(
                "WITH preferred AS (SELECT p.canonical_address AS address, p.display_name, p.formatted_address, p.first_contacted_at, p.last_contacted_at, p.send_count, s.send_count AS account_send_count, s.last_contacted_at AS account_last_contacted_at, s.account_id AS account_id, 0 AS hidden FROM contacted_people_account_stats s JOIN contacted_people p ON p.canonical_address = s.canonical_address WHERE s.account_id = ? AND p.hidden_at IS NULL AND EXISTS (SELECT 1 FROM contacted_people_account_stats enabled JOIN accounts a ON a.id = enabled.account_id WHERE enabled.canonical_address = p.canonical_address AND json_extract(a.data, '$.enabled') = 1) AND NOT EXISTS (SELECT 1 FROM accounts a WHERE lower(a.email) = p.canonical_address) ORDER BY s.last_contacted_at DESC, p.last_contacted_at DESC, p.canonical_address ASC LIMIT 8), fallback AS (SELECT p.canonical_address AS address, p.display_name, p.formatted_address, p.first_contacted_at, p.last_contacted_at, p.send_count, 0 AS account_send_count, NULL AS account_last_contacted_at, NULL AS account_id, 0 AS hidden FROM contacted_people p WHERE p.hidden_at IS NULL AND EXISTS (SELECT 1 FROM contacted_people_account_stats enabled JOIN accounts a ON a.id = enabled.account_id WHERE enabled.canonical_address = p.canonical_address AND json_extract(a.data, '$.enabled') = 1) AND NOT EXISTS (SELECT 1 FROM accounts a WHERE lower(a.email) = p.canonical_address) AND NOT EXISTS (SELECT 1 FROM contacted_people_account_stats s WHERE s.canonical_address = p.canonical_address AND s.account_id = ?) ORDER BY p.last_contacted_at DESC, p.canonical_address ASC LIMIT 8) SELECT address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, account_send_count, account_last_contacted_at, account_id, hidden FROM preferred UNION ALL SELECT address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, account_send_count, account_last_contacted_at, account_id, hidden FROM fallback LIMIT 8",
            )
            .bind(&preferred_account_id)
            .bind(&preferred_account_id)
            .fetch_all(&self.pool)
            .await?;
            return Ok(suggestions
                .into_iter()
                .filter(|suggestion| !self_addresses.contains(&suggestion.address))
                .collect());
        }
        // Match and rank in SQLite. This deliberately keeps the substring
        // fallback inside the database: it may scan local rows, but it never
        // copies an unbounded people history into Rust while the user types.
        // Every value is bound, including punctuation-only input.
        let word_prefix_probe = query
            .chars()
            .all(char::is_alphanumeric)
            .then(|| format!(" {query}"));
        Ok(sqlx::query_as::<_, ContactedPersonSuggestion>(
            "SELECT p.canonical_address AS address, p.display_name, p.formatted_address, p.first_contacted_at, p.last_contacted_at, p.send_count, COALESCE(s.send_count, 0) AS account_send_count, s.last_contacted_at AS account_last_contacted_at, s.account_id AS account_id, 0 AS hidden FROM contacted_people p LEFT JOIN contacted_people_account_stats s ON s.canonical_address = p.canonical_address AND s.account_id = ? WHERE p.hidden_at IS NULL AND EXISTS (SELECT 1 FROM contacted_people_account_stats enabled JOIN accounts a ON a.id = enabled.account_id WHERE enabled.canonical_address = p.canonical_address AND json_extract(a.data, '$.enabled') = 1) AND NOT EXISTS (SELECT 1 FROM accounts a WHERE lower(a.email) = p.canonical_address) AND (substr(COALESCE(NULLIF(p.normalized_address, ''), p.canonical_address), 1, length(?)) = ? OR (? IS NOT NULL AND (instr(p.normalized_display_tokens, ?) > 0 OR instr(p.normalized_address_tokens, ?) > 0)) OR instr(p.normalized_display_name, ?) > 0 OR instr(COALESCE(NULLIF(p.normalized_address, ''), p.canonical_address), ?) > 0) ORDER BY CASE WHEN substr(COALESCE(NULLIF(p.normalized_address, ''), p.canonical_address), 1, length(?)) = ? THEN 0 WHEN ? IS NOT NULL AND (instr(p.normalized_display_tokens, ?) > 0 OR instr(p.normalized_address_tokens, ?) > 0) THEN 1 ELSE 2 END, CASE WHEN COALESCE(s.send_count, 0) > 0 THEN 1 ELSE 0 END DESC, s.last_contacted_at DESC, p.send_count DESC, p.last_contacted_at DESC, p.canonical_address ASC LIMIT 8",
        )
        .bind(&preferred_account_id)
        .bind(&query)
        .bind(&query)
        .bind(&word_prefix_probe)
        .bind(&word_prefix_probe)
        .bind(&word_prefix_probe)
        .bind(&query)
        .bind(&query)
        .bind(&query)
        .bind(&query)
        .bind(&word_prefix_probe)
        .bind(&word_prefix_probe)
        .bind(&word_prefix_probe)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .filter(|suggestion| !self_addresses.contains(&suggestion.address))
        .collect())
    }

    /// Hides an address across every account without deleting its historical
    /// statistics. A later SMTP-accepted send to the exact address restores
    /// the suggestion as part of the same write transaction.
    pub async fn hide_contacted_person(&self, address: &str) -> Result<()> {
        let canonical_address = canonical_contacted_address(address)
            .ok_or_else(|| anyhow!("contacted person address is invalid"))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sequence = next_contacted_people_action_sequence_in_tx(&mut tx).await?;
        sqlx::query("UPDATE contacted_people SET hidden_at = ?, hidden_sequence = ? WHERE canonical_address = ?")
            .bind(Utc::now())
            .bind(sequence)
            .bind(canonical_address)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Clears all local autocomplete history while retaining durable backfill
    /// markers. The clear advances a provider-identity generation. Each Sent
    /// mailbox must subsequently capture its UIDVALIDITY and highest UID
    /// before provider-derived recipients may learn again.
    pub async fn clear_contacted_people(&self) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sequence = next_contacted_people_action_sequence_in_tx(&mut tx).await?;
        let cleared_at = Utc::now();
        // Keep the per-message markers. They are an idempotency boundary, not
        // user-visible history. The durable generation makes provider
        // backfill fail closed until a Sent SELECT captures a trusted UID
        // boundary, without using provider-controlled message timestamps.
        sqlx::query("DELETE FROM contacted_people")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('contacted_people_cleared_at', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(cleared_at.to_rfc3339())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('contacted_people_clear_sequence', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(sequence.to_string())
            .execute(&mut *tx)
            .await?;
        advance_contacted_people_collection_generation_in_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reserves a causal token immediately after SMTP acceptance. Callers
    /// must pass this token to the sequence-aware recorder before any later
    /// Sent append or UI action can interleave.
    pub async fn reserve_contacted_people_action_sequence(&self) -> Result<i64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sequence = next_contacted_people_action_sequence_in_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(sequence)
    }

    /// Whether composer and search person suggestions may use the local
    /// contacted-people index. This is enabled for existing profiles until a
    /// user explicitly turns it off.
    pub async fn autocomplete_suggestions_enabled(&self) -> Result<bool> {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'autocomplete_suggestions_enabled'",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(!matches!(value.as_deref(), Some("0") | Some("false")))
    }

    /// Persists the local autocomplete preference without altering already
    /// learned history. Each actual state transition advances the pending
    /// provider boundary, so messages that existed during a disabled period
    /// cannot appear after a later re-enable.
    pub async fn set_autocomplete_suggestions_enabled(&self, enabled: bool) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let was_enabled = autocomplete_suggestions_enabled_in_tx(&mut tx).await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES ('autocomplete_suggestions_enabled', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(if enabled { "1" } else { "0" })
            .execute(&mut *tx)
            .await?;
        if was_enabled != enabled {
            advance_contacted_people_collection_generation_in_tx(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Removes one account's contribution while retaining people that have
    /// been contacted from another still-configured account.
    pub async fn remove_contacted_people_account_contribution(
        &self,
        account_id: AccountId,
    ) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        remove_contacted_people_account_contribution_in_tx(&mut tx, &account_id.to_string())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Captures a Sent mailbox's provider identity boundary after a Clear or
    /// collection-setting transition. Call this immediately after SELECT has
    /// established `uid_validity` and the highest UID, before cataloguing any
    /// rows from that selected mailbox. Repeating the same UIDVALIDITY leaves
    /// the original cutoff unchanged; a UIDVALIDITY rollover replaces it.
    pub async fn capture_contacted_people_sent_provider_cutoff(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid_validity: u64,
        highest_uid: u64,
    ) -> Result<()> {
        if !is_contacted_people_sent_mailbox(mailbox) {
            return Err(anyhow!("contacted-people cutoff requires a Sent mailbox"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_live_contacted_people_account(&mut tx, &account_id).await?;
        let generation = contacted_people_collection_generation_in_tx(&mut tx).await?;
        if generation > 0 {
            sqlx::query("INSERT INTO contacted_people_sent_provider_cutoffs(account_id, mailbox, generation, uid_validity, cutoff_uid, captured_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, generation) DO UPDATE SET uid_validity = excluded.uid_validity, cutoff_uid = excluded.cutoff_uid, captured_at = excluded.captured_at WHERE contacted_people_sent_provider_cutoffs.uid_validity <> excluded.uid_validity")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(i64::try_from(uid_validity)?)
                .bind(i64::try_from(highest_uid)?)
                .bind(Utc::now())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Processes a bounded number of catalogued Sent rows. Each source
    /// message receives a durable marker in the same transaction as its
    /// recipient stats, so retries and interruptions cannot double count.
    /// Drafts and rows without a usable To/Cc/Bcc address are still marked
    /// complete without recipient writes, so they never cause an unbounded
    /// retry loop. Rows whose recipient headers have not yet been fetched are
    /// deliberately left unmarked: otherwise a header-only catalogue pass
    /// could permanently lose later Cc/Bcc data.
    pub async fn backfill_contacted_people_from_sent(
        &self,
        account_id: AccountId,
        excluded_addresses: &[String],
        limit: u32,
    ) -> Result<ContactedPeopleBackfillProgress> {
        let account_id = account_id.to_string();
        let limit = i64::from(limit.clamp(1, 500));
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_live_contacted_people_account(&mut tx, &account_id).await?;
        // Earlier builds could record a Sent source before the mailbox's
        // UIDVALIDITY was known. Promote that one deliberately-unknown source
        // to its now-trusted identity before selecting work. This prevents a
        // Message-ID-less row from being learned twice, while still allowing a
        // later UIDVALIDITY rollover to represent a new provider message.
        sqlx::query(
            "INSERT OR IGNORE INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) \
             SELECT m.account_id, m.mailbox, catalogue.uid_validity, m.uid \
             FROM messages m JOIN mailbox_catalog_state catalogue ON catalogue.account_id = m.account_id AND catalogue.mailbox = m.mailbox \
             WHERE m.account_id = ? AND m.recipient_headers_scanned = 1 \
             AND (m.mailbox = 'Sent' OR m.mailbox LIKE 'Sent::%') \
             AND EXISTS (SELECT 1 FROM contacted_people_backfill_sources unknown_source WHERE unknown_source.account_id = m.account_id AND unknown_source.mailbox = m.mailbox AND unknown_source.uid_validity = -1 AND unknown_source.uid = m.uid)",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await?;
        let rows: Vec<ContactedPeopleBackfillRow> = sqlx::query_as(
            "SELECT m.id AS message_id, m.message_id AS rfc_message_id, m.mailbox, m.uid, catalogue.uid_validity AS mailbox_uid_validity, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.is_draft, m.received_at FROM messages m LEFT JOIN mailbox_catalog_state catalogue ON catalogue.account_id = m.account_id AND catalogue.mailbox = m.mailbox WHERE m.account_id = ? AND m.recipient_headers_scanned = 1 AND (m.mailbox = 'Sent' OR m.mailbox LIKE 'Sent::%') AND NOT EXISTS (SELECT 1 FROM contacted_people_backfill_sources seen WHERE seen.account_id = m.account_id AND seen.mailbox = m.mailbox AND seen.uid_validity = COALESCE(catalogue.uid_validity, -1) AND seen.uid = m.uid) ORDER BY m.received_at ASC, m.id ASC LIMIT ?",
        )
        .bind(&account_id)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        // The scheduler checks this setting before calling us, but that check
        // can race with a user disabling collection. Recheck while holding
        // this write transaction immediately before any recipient write or
        // durable source-marker update. A disabled batch is a true no-op.
        if !autocomplete_suggestions_enabled_in_tx(&mut tx).await? {
            tx.rollback().await?;
            return Ok(ContactedPeopleBackfillProgress {
                processed_messages: 0,
                changed_people: 0,
                complete: false,
            });
        }
        let collection_generation = contacted_people_collection_generation_in_tx(&mut tx).await?;
        let provider_cutoffs = if collection_generation > 0 {
            contacted_people_sent_provider_cutoffs_in_tx(
                &mut tx,
                &account_id,
                collection_generation,
            )
            .await?
        } else {
            Vec::new()
        };
        let mut changed_people = 0;
        for row in &rows {
            let recipients = parse_contacted_people_headers(&[
                row.to_addresses.as_str(),
                row.cc_addresses.as_str(),
                row.bcc_addresses.as_str(),
            ]);
            let already_recorded_after_smtp = match row
                .rfc_message_id
                .as_deref()
                .and_then(normalize_message_id)
            {
                Some(rfc_message_id) => sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM contacted_people_outgoing_messages WHERE account_id = ? AND rfc_message_id = ?)",
                )
                .bind(&account_id)
                .bind(rfc_message_id)
                .fetch_one(&mut *tx)
                .await?,
                None => false,
            };
            let already_backfilled_rfc = match row
                .rfc_message_id
                .as_deref()
                .and_then(normalize_message_id)
            {
                Some(rfc_message_id) => sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM contacted_people_backfill_rfc_messages WHERE account_id = ? AND rfc_message_id = ?)",
                )
                .bind(&account_id)
                .bind(rfc_message_id)
                .fetch_one(&mut *tx)
                .await?,
                None => false,
            };
            // This is the critical interleaving guard. The legacy migration
            // may still be walking more than its startup 500 rows while the
            // Sent backfill starts. A legacy marker means this message's
            // contribution is already represented in the aggregate, so only
            // promote its source identity below and never add it again.
            let source_migration_pending = !contacted_people_migration_complete_in_tx(
                &mut tx,
                CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY,
            )
            .await?;
            let was_unresolved_legacy: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM contacted_people_legacy_unresolved_sources WHERE account_id = ? AND message_id = ?)",
            )
            .bind(&account_id)
            .bind(&row.message_id)
            .fetch_one(&mut *tx)
            .await?;
            let already_backfilled_legacy: bool = was_unresolved_legacy || (source_migration_pending && sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM contacted_people_backfill_messages WHERE account_id = ? AND message_id = ?)",
            )
            .bind(&account_id)
            .bind(&row.message_id)
            .fetch_one(&mut *tx)
            .await?);
            let is_after_provider_cutoff = collection_generation == 0
                || provider_cutoffs
                    .iter()
                    .find(|cutoff| cutoff.mailbox == row.mailbox)
                    .is_some_and(|cutoff| {
                        row.mailbox_uid_validity == Some(cutoff.uid_validity)
                            && row.uid > cutoff.cutoff_uid
                    });
            if !row.is_draft
                && is_after_provider_cutoff
                && !already_recorded_after_smtp
                && !already_backfilled_rfc
                && !already_backfilled_legacy
            {
                changed_people += record_contacted_people_in_tx(
                    &mut tx,
                    &account_id,
                    &recipients,
                    excluded_addresses,
                    row.received_at,
                    false,
                    None,
                )
                .await?;
            }
            sqlx::query("INSERT OR IGNORE INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)")
                .bind(&account_id)
                .bind(&row.message_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) VALUES (?, ?, ?, ?)")
                .bind(&account_id)
                .bind(&row.mailbox)
                .bind(row.mailbox_uid_validity.unwrap_or(-1))
                .bind(row.uid)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM contacted_people_legacy_unresolved_sources WHERE account_id = ? AND message_id = ?")
                .bind(&account_id)
                .bind(&row.message_id)
                .execute(&mut *tx)
                .await?;
            if let Some(rfc_message_id) =
                row.rfc_message_id.as_deref().and_then(normalize_message_id)
            {
                sqlx::query("INSERT OR IGNORE INTO contacted_people_backfill_rfc_messages(account_id, rfc_message_id) VALUES (?, ?)")
                    .bind(&account_id)
                    .bind(rfc_message_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let has_more: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages m LEFT JOIN mailbox_catalog_state catalogue ON catalogue.account_id = m.account_id AND catalogue.mailbox = m.mailbox WHERE m.account_id = ? AND m.recipient_headers_scanned = 1 AND (m.mailbox = 'Sent' OR m.mailbox LIKE 'Sent::%') AND NOT EXISTS (SELECT 1 FROM contacted_people_backfill_sources seen WHERE seen.account_id = m.account_id AND seen.mailbox = m.mailbox AND seen.uid_validity = COALESCE(catalogue.uid_validity, -1) AND seen.uid = m.uid))",
        )
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO contacted_people_backfill_progress(account_id, processed_messages, complete, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id) DO UPDATE SET processed_messages = contacted_people_backfill_progress.processed_messages + excluded.processed_messages, complete = excluded.complete, updated_at = excluded.updated_at")
            .bind(&account_id)
            .bind(i64::try_from(rows.len())?)
            .bind(!has_more)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ContactedPeopleBackfillProgress {
            processed_messages: rows.len(),
            changed_people,
            complete: !has_more,
        })
    }

    /// Deletes only provider-derived local mail state for an account. Account
    /// configuration and encrypted credentials remain intact so a subsequent
    /// full catalogue sync can rebuild from the authoritative provider.
    pub async fn reset_account_mail_index(&self, id: AccountId) -> Result<()> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // A reset invalidates every remote locator and local flag for this
        // account. Keep the invalidation in this transaction so a provider
        // search cannot write a result between a separate generation bump and
        // the destructive reset.
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) SELECT ?, 0 WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?)")
            .bind(&account_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?;
        let advanced = sqlx::query("UPDATE account_search_generations SET generation = generation + 1 WHERE account_id = ?")
            .bind(&account_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if advanced != 1 {
            tx.rollback().await?;
            return Err(anyhow!("account does not exist"));
        }
        for statement in [
            "DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM mailbox_catalog_state WHERE account_id = ?",
            "DELETE FROM mailbox_snapshot_generations WHERE account_id = ?",
            "DELETE FROM mailbox_sync_failures WHERE account_id = ?",
            "DELETE FROM mailbox_sync_state WHERE account_id = ?",
            "DELETE FROM mailbox_action_tombstones WHERE account_id = ?",
            "DELETE FROM sent_correspondents WHERE account_id = ?",
            "DELETE FROM messages WHERE account_id = ?",
        ] {
            sqlx::query(statement)
                .bind(&account_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_messages(&self, messages: &[MailSummary]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for message in messages {
            persist_message(&mut tx, message).await?;
        }
        tx.commit().await?;
        self.rebuild_threads_for_messages(messages).await?;
        Ok(())
    }

    /// Catalogue sync already assigns a deterministic provisional thread id
    /// from References/In-Reply-To. Avoid rebuilding every account-wide
    /// disjoint set for each small publication batch; the sync performs one
    /// authoritative rebuild after the historical pass completes.
    pub async fn upsert_catalog_messages(&self, messages: &[MailSummary]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for message in messages {
            persist_message(&mut tx, message).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Publishes catalogue metadata from a replacement UIDVALIDITY namespace.
    ///
    /// A UID is only unique within one UIDVALIDITY value. Generic catalogue
    /// writes intentionally preserve complete local content when a headers-
    /// only refresh collides with an existing UID, but that policy would leak
    /// the old namespace's snippet, body, classification, and attachment
    /// state into a recycled UID. Replacement publication therefore removes
    /// the exact old locator and all of its dependents before inserting the
    /// new provider row.
    pub async fn replace_uidvalidity_catalog_messages(
        &self,
        messages: &[MailSummary],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        replace_uidvalidity_catalog_messages_in_transaction(&mut tx, messages).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Provider-search-only catalogue publication. The generation comparison
    /// and writes share one immediate transaction, closing the gap between a
    /// caller's last cancellation check and its stale IMAP response write.
    /// Returns false without changing state when a foreground account mutation
    /// has advanced the generation.
    pub async fn upsert_catalog_messages_if_account_generation(
        &self,
        account_id: AccountId,
        generation: i64,
        messages: &[MailSummary],
    ) -> Result<bool> {
        let account_key = account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_key)
        {
            return Err(anyhow!("catalogue batch does not belong to this account"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !account_search_generation_matches_in_tx(&mut tx, &account_key, generation).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        for message in messages {
            persist_message(&mut tx, message).await?;
        }
        tx.commit().await?;
        Ok(true)
    }
    /// Captures local flags before a remote catalogue fetch. The returned
    /// values are later used as compare-and-swap preconditions at publication.
    pub async fn capture_recent_catalogue_expected_flags(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uids: &[u32],
    ) -> Result<Vec<ExpectedMessageFlags>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; uids.len()].join(",");
        let sql = format!(
            "SELECT account_id, mailbox, uid, is_read, is_flagged FROM messages WHERE account_id = ? AND mailbox = ? AND uid IN ({placeholders})"
        );
        let mut statement = sqlx::query_as::<_, ExpectedMessageFlags>(&sql)
            .bind(account_id.to_string())
            .bind(mailbox);
        for uid in uids {
            statement = statement.bind(i64::from(*uid));
        }
        Ok(statement.fetch_all(&self.pool).await?)
    }

    /// Publishes a delayed recent-catalogue refresh. Provider flags apply to
    /// new rows, but conflict rows accept them only when their current local
    /// flags still match the snapshot captured before the remote fetch.
    pub async fn upsert_recent_catalog_messages(
        &self,
        messages: &[MailSummary],
        expected_flags: &[ExpectedMessageFlags],
    ) -> Result<()> {
        let expected_by_locator: HashMap<(&str, &str, i64), (bool, bool)> = expected_flags
            .iter()
            .map(|expected| {
                (
                    (
                        expected.account_id.as_str(),
                        expected.mailbox.as_str(),
                        expected.uid,
                    ),
                    (expected.is_read, expected.is_flagged),
                )
            })
            .collect();
        let mut tx = self.pool.begin().await?;
        for message in messages {
            persist_message_with_flag_policy(
                &mut tx,
                message,
                FlagUpdatePolicy::CompareAndSwap(
                    expected_by_locator
                        .get(&(
                            message.account_id.as_str(),
                            message.mailbox.as_str(),
                            message.uid,
                        ))
                        .copied(),
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stores one incremental mailbox batch and returns messages eligible for
    /// new-mail notification. The first successful batch establishes a silent
    /// baseline so connecting an account never alerts for historical mail.
    pub async fn save_synced_messages(
        &self,
        account_id: AccountId,
        mailbox: &str,
        messages: &[MailSummary],
    ) -> Result<Vec<MailSummary>> {
        let watermark = messages
            .iter()
            .map(|message| u32::try_from(message.uid).context("message UID is invalid"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max();
        self.save_synced_messages_through(account_id, mailbox, messages, watermark)
            .await
    }

    /// Stores a realtime batch while advancing only through the caller's
    /// highest contiguous successful UID. A higher message may be committed
    /// for immediate display without making an earlier failed UID invisible
    /// to the next retry.
    pub async fn save_synced_messages_through(
        &self,
        account_id: AccountId,
        mailbox: &str,
        messages: &[MailSummary],
        watermark: Option<u32>,
    ) -> Result<Vec<MailSummary>> {
        let account_id = account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_id || message.mailbox != mailbox)
        {
            return Err(anyhow!(
                "sync batch message does not match the requested account or mailbox"
            ));
        }
        // Keep the existence check and all provider-derived writes in one
        // transaction.  This prevents a late realtime cycle from reporting
        // arrivals for an account removed between its IMAP fetch and local
        // publication.
        // Two independent Store connections can both begin a deferred read
        // transaction before either publishes. Acquire the SQLite write lease
        // first so the loser waits rather than failing while upgrading its
        // read lock to a write lock.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_removed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
        if account_removed {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        let account_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
                .bind(&account_id)
                .fetch_one(&mut *tx)
                .await?;
        if !account_exists {
            tx.rollback().await?;
            return Err(anyhow!("account does not exist"));
        }
        let initialized: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_one(&mut *tx)
        .await?;
        let existing_uids: HashSet<i64> =
            sqlx::query_scalar("SELECT uid FROM messages WHERE account_id = ? AND mailbox = ?")
                .bind(&account_id)
                .bind(mailbox)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        for message in messages {
            persist_message(&mut tx, message).await?;
        }
        sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET initialized_at=excluded.initialized_at, highest_uid=MAX(COALESCE(mailbox_sync_state.highest_uid, 0), COALESCE(excluded.highest_uid, 0))")
            .bind(&account_id)
            .bind(mailbox)
            .bind(Utc::now())
            .bind(watermark.map(i64::from))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.rebuild_threads_for_account(&account_id).await?;

        Ok(if initialized && mailbox == "INBOX" {
            messages
                .iter()
                .filter(|message| !message.is_read && !existing_uids.contains(&message.uid))
                .cloned()
                .collect()
        } else {
            Vec::new()
        })
    }

    pub async fn replace_mailbox_messages(
        &self,
        account_id: AccountId,
        mailbox: &str,
        messages: &[MailSummary],
    ) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .execute(&self.pool)
            .await?;
        self.upsert_messages(messages).await
    }

    /// Returns the highest UID we have stored for a mailbox. IMAP assigns UIDs
    /// monotonically within a mailbox, so this is a durable sync watermark.
    pub async fn highest_mailbox_uid(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Option<u32>> {
        let (uid,): (Option<i64>,) =
            sqlx::query_as("SELECT COALESCE((SELECT highest_uid FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?), (SELECT MAX(uid) FROM messages WHERE account_id = ? AND mailbox = ?))")
                .bind(account_id.to_string())
                .bind(mailbox)
                .bind(account_id.to_string())
                .bind(mailbox)
                .fetch_one(&self.pool)
                .await?;
        uid.map(|uid| u32::try_from(uid).context("stored message UID is invalid"))
            .transpose()
    }

    pub async fn mailbox_uids(&self, account_id: AccountId, mailbox: &str) -> Result<HashSet<u32>> {
        let rows: Vec<i64> =
            sqlx::query_scalar("SELECT uid FROM messages WHERE account_id = ? AND mailbox = ?")
                .bind(account_id.to_string())
                .bind(mailbox)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .filter_map(|uid| u32::try_from(uid).ok())
            .collect())
    }

    pub async fn reconcile_mailbox_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_uids: &HashSet<u32>,
    ) -> Result<u64> {
        let local = self.mailbox_uids(account_id, mailbox).await?;
        let stale: Vec<u32> = local.difference(remote_uids).copied().collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let mut removed = 0;
        for uid in stale {
            removed += sqlx::query(
                "DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
            )
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(uid)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        if removed > 0 {
            self.rebuild_threads_for_account(&account_id.to_string())
                .await?;
        }
        Ok(removed)
    }

    pub async fn update_mailbox_flags(
        &self,
        account_id: AccountId,
        mailbox: &str,
        flags: &[(u32, bool, bool)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (uid, is_read, is_flagged) in flags {
            sqlx::query("UPDATE messages SET is_read = ?, is_flagged = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(is_read)
                .bind(is_flagged)
                .bind(account_id.to_string())
                .bind(mailbox)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
            if *is_flagged {
                sqlx::query("DELETE FROM message_content_cache WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
                    .bind(account_id.to_string())
                    .bind(mailbox)
                    .bind(i64::from(*uid))
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
                    .bind(account_id.to_string())
                    .bind(mailbox)
                    .bind(i64::from(*uid))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
                    .bind(account_id.to_string())
                    .bind(mailbox)
                    .bind(i64::from(*uid))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn mailbox_catalog_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Option<MailboxCatalogState>> {
        Ok(sqlx::query_as::<_, MailboxCatalogState>(
            "SELECT account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Returns every local catalogue state backed by a provider mailbox. One
    /// provider mailbox can have multiple local locators, all of which need a
    /// tombstone when a remote move succeeds.
    pub async fn mailbox_catalog_states_for_remote(
        &self,
        account_id: AccountId,
        remote_name: &str,
    ) -> Result<Vec<MailboxCatalogState>> {
        Ok(sqlx::query_as::<_, MailboxCatalogState>(
            "SELECT account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq FROM mailbox_catalog_state WHERE account_id = ? AND LOWER(remote_name) = LOWER(?) ORDER BY CASE mailbox WHEN 'INBOX' THEN 0 WHEN 'Archive' THEN 1 WHEN 'Spam' THEN 2 ELSE 3 END, mailbox",
        )
        .bind(account_id.to_string())
        .bind(remote_name)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn save_mailbox_catalog_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        remote_total: usize,
        historical_complete: bool,
    ) -> Result<()> {
        self.save_mailbox_catalog_state_with_generation(
            account_id,
            MailboxCatalogStateWrite {
                mailbox,
                remote_name,
                uid_validity,
                remote_total,
                historical_complete,
            },
            None,
        )
        .await
        .map(|_| ())
    }

    /// Returns false without publication if a foreground mutation advanced
    /// the account generation while provider search was in flight.
    #[expect(
        clippy::too_many_arguments,
        reason = "This established public Store API has separate scalar arguments for the mailbox snapshot it publishes."
    )]
    pub async fn save_mailbox_catalog_state_if_account_generation(
        &self,
        account_id: AccountId,
        generation: i64,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        remote_total: usize,
        historical_complete: bool,
    ) -> Result<bool> {
        self.save_mailbox_catalog_state_with_generation(
            account_id,
            MailboxCatalogStateWrite {
                mailbox,
                remote_name,
                uid_validity,
                remote_total,
                historical_complete,
            },
            Some(generation),
        )
        .await
    }

    async fn save_mailbox_catalog_state_with_generation(
        &self,
        account_id: AccountId,
        state: MailboxCatalogStateWrite<'_>,
        expected_generation: Option<i64>,
    ) -> Result<bool> {
        let account_key = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(generation) = expected_generation {
            if !account_search_generation_matches_in_tx(&mut tx, &account_key, generation).await? {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, remote_total=excluded.remote_total, historical_complete=excluded.historical_complete, updated_at=excluded.updated_at")
            .bind(&account_key)
            .bind(state.mailbox)
            .bind(state.remote_name)
            .bind(state.uid_validity)
            .bind(state.remote_total as i64)
            .bind(state.historical_complete)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn reset_mailbox_catalog(&self, account_id: AccountId, mailbox: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.rebuild_threads_for_account(&account_id.to_string())
            .await?;
        Ok(())
    }

    /// Removes a mailbox namespace only after the provider has authoritatively
    /// reported it nonexistent. This is transactional so a provider mapping
    /// reset cannot leave old Sent, Archive, Spam, or cache rows visible.
    pub async fn clear_nonexistent_mailbox_namespace(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for table in [
            "message_content_fetches",
            "message_content_cache",
            "starred_attachment_metadata",
            "starred_message_bodies",
            "attachments",
        ] {
            let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)");
            sqlx::query(&statement)
                .bind(&account_id)
                .bind(mailbox)
                .execute(&mut *tx)
                .await?;
        }
        for statement in [
            "DELETE FROM messages WHERE account_id = ? AND mailbox = ?",
            "DELETE FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
            "DELETE FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?",
            "DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ?",
            "DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ?",
            "DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?",
        ] {
            sqlx::query(statement)
                .bind(&account_id)
                .bind(mailbox)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.rebuild_threads_for_account(&account_id).await?;
        Ok(())
    }

    /// After every authoritative plan for one special-use family has
    /// completed, removes family keys which are absent from that plan's keep
    /// set. A single mailbox finalization deliberately never calls this: a
    /// provider can expose several valid Sent or Trash siblings at once.
    /// An empty keep set is an authoritative empty family.
    pub async fn prune_obsolete_mailbox_family_namespaces(
        &self,
        account_id: AccountId,
        family: &str,
        keep_mailboxes: &[String],
    ) -> Result<()> {
        if family.is_empty() || mailbox_family(family) != family {
            return Err(anyhow!("mailbox family must be a non-empty root key"));
        }
        if keep_mailboxes
            .iter()
            .any(|mailbox| mailbox_family(mailbox) != family)
        {
            return Err(anyhow!("mailbox keep set contains another family"));
        }
        let account_id = account_id.to_string();
        let descendants = format!("{family}::%");
        let keep_clause = if keep_mailboxes.is_empty() {
            String::new()
        } else {
            format!(
                " AND mailbox NOT IN ({})",
                std::iter::repeat_n("?", keep_mailboxes.len())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let predicate = format!("account_id = ? AND (mailbox = ? OR mailbox LIKE ?){keep_clause}");
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for table in [
            "message_content_fetches",
            "message_content_cache",
            "starred_attachment_metadata",
            "starred_message_bodies",
            "attachments",
        ] {
            let statement = format!(
                "DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE {predicate})"
            );
            let mut query = sqlx::query(&statement)
                .bind(&account_id)
                .bind(family)
                .bind(&descendants);
            for mailbox in keep_mailboxes {
                query = query.bind(mailbox);
            }
            query.execute(&mut *tx).await?;
        }
        for table in [
            "messages",
            "mailbox_catalog_state",
            "mailbox_sync_state",
            "mailbox_snapshot_generations",
            "mailbox_sync_failures",
            "mailbox_action_tombstones",
        ] {
            let statement = format!("DELETE FROM {table} WHERE {predicate}");
            let mut query = sqlx::query(&statement)
                .bind(&account_id)
                .bind(family)
                .bind(&descendants);
            for mailbox in keep_mailboxes {
                query = query.bind(mailbox);
            }
            query.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.rebuild_threads_for_account(&account_id).await?;
        Ok(())
    }

    /// Reconciles the durable UIDVALIDITY/watermark before incremental sync.
    /// A changed UIDVALIDITY invalidates only pending incremental work. The
    /// previous committed sync/catalogue identity survives until replacement
    /// finalization, so retries and restarts still require a full replacement.
    pub async fn prepare_mailbox_sync(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid_validity: Option<u64>,
    ) -> Result<MailboxSyncState> {
        let account_id = account_id.to_string();
        let row: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT initialized_at, highest_uid, uid_validity FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await?;
        let catalog_uid_validity: Option<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        let stored_uid_validity = row
            .as_ref()
            .and_then(|(_, _, stored)| *stored)
            .or(catalog_uid_validity);
        let changed = stored_uid_validity
            .zip(uid_validity.and_then(|value| i64::try_from(value).ok()))
            .is_some_and(|(stored, current)| stored != current);
        if changed {
            let mut tx = self.pool.begin().await?;
            // Locators are scoped by UIDVALIDITY. Keep committed metadata
            // visible until the replacement generation succeeds, but never
            // let cached bodies or attachment bytes from the old namespace be
            // served for a recycled UID.
            for statement in [
                "DELETE FROM message_content_cache WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)",
                "DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)",
                "DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)",
                "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)",
            ] {
                sqlx::query(statement)
                    .bind(&account_id)
                    .bind(mailbox)
                    .execute(&mut *tx)
                    .await?;
            }
            let current_uid_validity = uid_validity.and_then(|value| i64::try_from(value).ok());
            let replacement_generation_matches = match current_uid_validity {
                Some(current_uid_validity) => sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND uid_validity = ?)",
                )
                .bind(&account_id)
                .bind(mailbox)
                .bind(current_uid_validity)
                .fetch_one(&mut *tx)
                .await?,
                None => false,
            };
            if !replacement_generation_matches {
                if let Some(current_uid_validity) = current_uid_validity {
                    // A snapshot for another namespace is proven stale. Keep a
                    // matching replacement generation across reconnects.
                    sqlx::query("DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND uid_validity != ?")
                        .bind(&account_id)
                        .bind(mailbox)
                        .bind(current_uid_validity)
                        .execute(&mut *tx)
                        .await?;
                }
                // Failures belong to the previous namespace only until a
                // replacement generation is active; later retries preserve
                // its retry queue alongside its staged outcomes.
                sqlx::query(
                    "DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ?",
                )
                .bind(&account_id)
                .bind(mailbox)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(MailboxSyncState {
                initialized: false,
                highest_uid: None,
                uid_validity,
                uid_validity_changed: true,
            });
        }
        if let Some(uid_validity) = uid_validity.and_then(|value| i64::try_from(value).ok()) {
            sqlx::query("UPDATE mailbox_sync_state SET uid_validity = COALESCE(uid_validity, ?) WHERE account_id = ? AND mailbox = ?")
                .bind(uid_validity)
                .bind(&account_id)
                .bind(mailbox)
                .execute(&self.pool)
                .await?;
        }
        Ok(MailboxSyncState {
            initialized: row.is_some(),
            highest_uid: row
                .as_ref()
                .and_then(|(_, uid, _)| *uid)
                .and_then(|uid| u32::try_from(uid).ok()),
            uid_validity,
            uid_validity_changed: false,
        })
    }

    pub async fn set_mailbox_uid_validity(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid_validity: Option<u64>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE mailbox_sync_state SET uid_validity = ? WHERE account_id = ? AND mailbox = ?",
        )
        .bind(uid_validity.and_then(|value| i64::try_from(value).ok()))
        .bind(account_id.to_string())
        .bind(mailbox)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_message_content_state(&self, id: &str, state: &str) -> Result<()> {
        if !matches!(state, "headers_only" | "hydrating" | "complete" | "failed") {
            return Err(anyhow!("invalid message content state"));
        }
        sqlx::query("UPDATE messages SET content_state = ? WHERE id = ?")
            .bind(state)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn claim_message_hydration(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE messages SET content_state = 'hydrating' WHERE id = ? AND content_state IN ('headers_only', 'failed')")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn message(&self, id: &str) -> Result<Option<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id = ?";
        let mut message = sqlx::query_as::<_, MailSummary>(SQL)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(message) = message.as_mut() {
            if let Some((body_text, body_html)) = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT body_text, body_html FROM starred_message_bodies WHERE message_id = ? AND attachment_presentation_version = ?",
            )
            .bind(id)
            .bind(ATTACHMENT_PRESENTATION_VERSION)
            .fetch_optional(&self.pool)
            .await?
            {
                message.body_text = body_text;
                message.body_html = body_html;
                message.content_state = "complete".into();
            }
        }
        Ok(message)
    }

    pub async fn set_message_flagged(&self, id: &str, flagged: bool) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_id: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM messages WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(account_id) = account_id else {
            tx.rollback().await?;
            return Err(anyhow!("message does not exist"));
        };
        advance_account_search_generation_in_tx(&mut tx, &account_id).await?;
        sqlx::query("UPDATE messages SET is_flagged = ? WHERE id = ?")
            .bind(flagged)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if flagged {
            sqlx::query("DELETE FROM message_content_cache WHERE message_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM starred_message_bodies WHERE message_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_message_read(&self, id: &str, read: bool) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_id: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM messages WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(account_id) = account_id else {
            tx.rollback().await?;
            return Err(anyhow!("message does not exist"));
        };
        advance_account_search_generation_in_tx(&mut tx, &account_id).await?;
        sqlx::query("UPDATE messages SET is_read = ? WHERE id = ?")
            .bind(read)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn starred_attachment_metadata(&self, message_id: &str) -> Result<Vec<Attachment>> {
        Ok(sqlx::query_as::<_, Attachment>("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe FROM starred_attachment_metadata WHERE message_id = ? AND presentation IN ('downloadable', 'both') ORDER BY filename, id")
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn starred_body(&self, message_id: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(sqlx::query_as(
            "SELECT body_text, body_html FROM starred_message_bodies WHERE message_id = ? AND attachment_presentation_version = ?",
        )
        .bind(message_id)
        .bind(ATTACHMENT_PRESENTATION_VERSION)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records the attachment state observed during an authoritative remote
    /// fetch without changing the message locator, flags, or cached body.
    /// Returns false when the local message disappeared before the fetch
    /// completed, preventing a stale provider response from reviving it.
    pub async fn update_message_attachment_state(
        &self,
        message_id: &str,
        has_attachments: bool,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE messages SET has_attachments = ? WHERE id = ?")
            .bind(has_attachments)
            .bind(message_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(updated == 1)
    }

    /// Promotes freshly fetched content into the durable starred cache only
    /// while the same local message remains starred. It intentionally does
    /// not update message flags or any provider identity fields.
    pub async fn cache_starred_message_content(
        &self,
        message_id: &str,
        content: CachedMessageContent,
    ) -> Result<bool> {
        let mut attachments = content.attachments;
        for attachment in &mut attachments {
            attachment.message_id = message_id.to_owned();
        }
        attachments.retain(|attachment| attachment.presentation.is_downloadable());

        let mut tx = self.pool.begin().await?;
        let still_flagged: Option<bool> =
            sqlx::query_scalar("SELECT is_flagged FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if still_flagged != Some(true) {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO starred_message_bodies(message_id, body_text, body_html, attachment_presentation_version, cached_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(message_id) DO UPDATE SET body_text=excluded.body_text, body_html=excluded.body_html, attachment_presentation_version=excluded.attachment_presentation_version, cached_at=excluded.cached_at")
            .bind(message_id)
            .bind(&content.body_text)
            .bind(&content.body_html)
            .bind(ATTACHMENT_PRESENTATION_VERSION)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id = ?")
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        for attachment in attachments {
            sqlx::query("INSERT INTO starred_attachment_metadata(id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(&attachment.id)
                .bind(message_id)
                .bind(&attachment.filename)
                .bind(&attachment.mime_type)
                .bind(attachment.size_bytes)
                .bind(attachment.is_inline)
                .bind(attachment.presentation)
                .bind(attachment.is_potentially_unsafe)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Returns a complete non-starred foreground cache entry and promotes it
    /// in the persistent LRU. Starred content intentionally bypasses this
    /// cache so the durable starred cache remains authoritative.
    pub async fn cached_message_content(
        &self,
        message_id: &str,
    ) -> Result<Option<CachedMessageContent>> {
        let mut tx = self.pool.begin().await?;
        let cached: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT c.body_text, c.body_html, c.unsubscribe_kind, c.attachments_json FROM message_content_cache c JOIN messages m ON m.id = c.message_id WHERE c.message_id = ? AND c.content_state = 'complete' AND m.is_flagged = 0",
        )
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((body_text, body_html, unsubscribe_kind, attachments_json)) = cached else {
            return Ok(None);
        };
        let attachments: Vec<Attachment> = match serde_json::from_str(&attachments_json) {
            Ok(attachments) => attachments,
            Err(error) => {
                tracing::warn!(
                    %error,
                    %message_id,
                    "discarding corrupt cached attachment metadata"
                );
                sqlx::query("DELETE FROM message_content_cache WHERE message_id = ?")
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(None);
            }
        };
        if attachments
            .iter()
            .any(|attachment| !attachment.presentation.is_current())
        {
            // A legacy `is_inline` bit cannot tell whether a resource was
            // referenced by the selected HTML branch. Re-fetch instead of
            // exposing a plausible but incorrect attachment list.
            sqlx::query("DELETE FROM message_content_cache WHERE message_id = ?")
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(None);
        }
        // Use a monotonic sequence instead of wall-clock time so closely
        // spaced opens still have deterministic LRU ordering.
        sqlx::query(
            "UPDATE message_content_cache SET last_accessed = (SELECT COALESCE(MAX(last_accessed), 0) + 1 FROM message_content_cache) WHERE message_id = ?",
        )
        .bind(message_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(CachedMessageContent {
            body_text,
            body_html,
            unsubscribe_kind,
            attachments,
        }))
    }

    /// Stores only a successful, complete foreground fetch for a currently
    /// non-starred message. Oversized replacements remove an older cache row
    /// rather than leaving stale content available on a later open.
    pub async fn cache_message_content(
        &self,
        message_id: &str,
        is_flagged: bool,
        content: CachedMessageContent,
    ) -> Result<()> {
        self.cache_message_content_with_budget(
            message_id,
            is_flagged,
            content,
            MESSAGE_CONTENT_CACHE_MAX_BYTES,
        )
        .await
    }

    /// Stores an authoritative complete text-part response obtained while
    /// searching. This is intentionally separate from `message_content_cache`:
    /// it never records HTML, attachment metadata, or a reader-complete state.
    /// Returns `false` if the message disappeared or the text exceeds the
    /// search-cache budget before it could be stored.
    pub async fn cache_search_body_text(&self, message_id: &str, body_text: &str) -> Result<bool> {
        self.cache_search_body_text_with_budget(
            message_id,
            body_text,
            MESSAGE_SEARCH_BODY_TEXT_CACHE_MAX_BYTES,
            None,
        )
        .await
    }

    /// Generation-bound counterpart used only for provider search results.
    /// It cannot overwrite a newer reader/search cache after an account
    /// mutation has become authoritative.
    pub async fn cache_search_body_text_if_account_generation(
        &self,
        account_id: AccountId,
        generation: i64,
        message_id: &str,
        body_text: &str,
    ) -> Result<bool> {
        self.cache_search_body_text_with_budget(
            message_id,
            body_text,
            MESSAGE_SEARCH_BODY_TEXT_CACHE_MAX_BYTES,
            Some((account_id.to_string(), generation)),
        )
        .await
    }

    async fn cache_search_body_text_with_budget(
        &self,
        message_id: &str,
        body_text: &str,
        max_bytes: i64,
        expected_generation: Option<(String, i64)>,
    ) -> Result<bool> {
        let byte_size = i64::try_from(body_text.len()).context("search body text is too large")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some((account_id, generation)) = expected_generation.as_ref() {
            if !account_search_generation_matches_in_tx(&mut tx, account_id, *generation).await? {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        if byte_size > max_bytes {
            sqlx::query("DELETE FROM message_search_body_text WHERE message_id = ?")
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(false);
        }

        let stored = sqlx::query(
            "INSERT INTO message_search_body_text(message_id, body_text, byte_size, last_indexed) SELECT ?, ?, ?, (SELECT COALESCE(MAX(last_indexed), 0) + 1 FROM message_search_body_text) WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?) ON CONFLICT(message_id) DO UPDATE SET body_text = excluded.body_text, byte_size = excluded.byte_size, last_indexed = excluded.last_indexed",
        )
        .bind(message_id)
        .bind(body_text)
        .bind(byte_size)
        .bind(message_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if stored == 0 {
            tx.rollback().await?;
            return Ok(false);
        }

        loop {
            let used_bytes: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(byte_size), 0) FROM message_search_body_text",
            )
            .fetch_one(&mut *tx)
            .await?;
            if used_bytes <= max_bytes {
                break;
            }
            let removed = sqlx::query(
                "DELETE FROM message_search_body_text WHERE message_id = (SELECT message_id FROM message_search_body_text ORDER BY last_indexed, message_id LIMIT 1)",
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if removed == 0 {
                break;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Returns text cached by a provider search, without presenting it as
    /// reader-ready content or updating its eviction order.
    pub async fn cached_search_body_text(&self, message_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT body_text FROM message_search_body_text WHERE message_id = ?",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn cache_message_content_with_budget(
        &self,
        message_id: &str,
        is_flagged: bool,
        content: CachedMessageContent,
        max_bytes: i64,
    ) -> Result<()> {
        let mut attachments = content.attachments;
        // The cache key is the current provider locator. Never retain parsed
        // metadata that refers to a different local message id.
        for attachment in &mut attachments {
            attachment.message_id = message_id.to_owned();
        }
        attachments.retain(|attachment| attachment.presentation.is_downloadable());
        let attachments_json = serde_json::to_string(&attachments)?;
        let byte_size = cache_entry_byte_size(
            &content.body_text,
            content.body_html.as_deref(),
            content.unsubscribe_kind.as_deref(),
            &attachments_json,
        )?;
        if byte_size > max_bytes {
            sqlx::query("DELETE FROM message_content_cache WHERE message_id = ?")
                .bind(message_id)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        let stored = sqlx::query(
            "INSERT INTO message_content_cache(message_id, content_state, body_text, body_html, unsubscribe_kind, attachments_json, byte_size, last_accessed) SELECT ?, 'complete', ?, ?, ?, ?, ?, (SELECT COALESCE(MAX(last_accessed), 0) + 1 FROM message_content_cache) WHERE ? = 0 AND EXISTS (SELECT 1 FROM messages WHERE id = ? AND is_flagged = 0) ON CONFLICT(message_id) DO UPDATE SET content_state = excluded.content_state, body_text = excluded.body_text, body_html = excluded.body_html, unsubscribe_kind = excluded.unsubscribe_kind, attachments_json = excluded.attachments_json, byte_size = excluded.byte_size, last_accessed = excluded.last_accessed",
        )
        .bind(message_id)
        .bind(&content.body_text)
        .bind(&content.body_html)
        .bind(&content.unsubscribe_kind)
        .bind(&attachments_json)
        .bind(byte_size)
        .bind(is_flagged)
        .bind(message_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if stored == 0 {
            // A concurrent flag change makes the starred cache authoritative;
            // a deleted message has already cascaded this row away.
            sqlx::query("DELETE FROM message_content_cache WHERE message_id = ?")
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        }

        let recent_cutoff =
            Utc::now() - chrono::Duration::days(MESSAGE_CONTENT_CACHE_RECENT_WINDOW_DAYS);
        loop {
            let used_bytes: i64 =
                sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM message_content_cache")
                    .fetch_one(&mut *tx)
                    .await?;
            if used_bytes <= max_bytes {
                break;
            }
            let removed = sqlx::query(
                "DELETE FROM message_content_cache WHERE message_id = (SELECT message_id FROM (SELECT c.message_id FROM message_content_cache c JOIN messages m ON m.id = c.message_id ORDER BY CASE WHEN m.mailbox IN ('INBOX', 'Sent', 'Archive') AND m.received_at >= ? THEN 1 ELSE 0 END, c.last_accessed, c.message_id LIMIT 1))",
            )
            .bind(recent_cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if removed == 0 {
                break;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Returns recent primary-folder messages that still need their body in
    /// the cache appropriate to their current flag state. Exact, non-empty
    /// stored Message-IDs are deduplicated in SQL; malformed or variant IDs
    /// deliberately remain distinct for the coordinator to deduplicate.
    pub async fn recent_body_cache_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<MailSummary>> {
        self.recent_body_cache_candidates_page(account_id, cutoff, limit, 0)
            .await
    }

    /// Returns a deterministic page of recent body-cache candidates. The
    /// duplicate ranking runs before pagination so pages neither overlap nor
    /// resurrect a lower-ranked copy of an already-selected Message-ID.
    pub async fn recent_body_cache_candidates_page(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "WITH uncached AS (SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals, ROW_NUMBER() OVER (PARTITION BY CASE WHEN m.message_id IS NULL OR trim(m.message_id) = '' THEN m.id ELSE m.message_id END ORDER BY m.received_at DESC, m.id DESC) AS duplicate_rank FROM messages m LEFT JOIN message_content_cache c ON c.message_id = m.id LEFT JOIN starred_message_bodies b ON b.message_id = m.id AND b.attachment_presentation_version = ? WHERE m.account_id = ? AND m.mailbox IN ('INBOX', 'Sent', 'Archive') AND m.received_at >= ? AND ((m.is_flagged = 0 AND c.message_id IS NULL) OR (m.is_flagged = 1 AND b.message_id IS NULL))) SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM uncached WHERE duplicate_rank = 1 ORDER BY received_at DESC, id DESC LIMIT ? OFFSET ?";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(ATTACHMENT_PRESENTATION_VERSION)
            .bind(account_id.to_string())
            .bind(cutoff)
            .bind(i64::from(limit))
            .bind(i64::from(offset))
            .fetch_all(&self.pool)
            .await?)
    }

    /// Claims a local message for an in-flight body fetch. Claims are
    /// transient leases shared by every process using the profile. A stale
    /// lease may be replaced after the bounded foreground wait window, while
    /// the owner token prevents an old guard from releasing its replacement.
    pub async fn claim_message_content_fetch(&self, message_id: &str) -> Result<bool> {
        self.claim_message_content_fetch_for_owner(message_id, message_content_fetch_owner())
            .await
    }

    async fn claim_message_content_fetch_for_owner(
        &self,
        message_id: &str,
        owner: &str,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "INSERT INTO message_content_fetches(message_id, claimed_at, claim_owner) SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?) ON CONFLICT(message_id) DO UPDATE SET claimed_at = excluded.claimed_at, claim_owner = excluded.claim_owner WHERE message_content_fetches.claimed_at <= ?",
        )
        .bind(message_id)
        .bind(Utc::now())
        .bind(owner)
        .bind(message_id)
        .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS))
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn acquire_message_content_fetch(
        &self,
        message_id: &str,
    ) -> Result<Option<MessageContentFetchClaim>> {
        let owner = uuid::Uuid::new_v4().to_string();
        if !self
            .claim_message_content_fetch_for_owner(message_id, &owner)
            .await?
        {
            return Ok(None);
        }
        let renewal_store = self.clone();
        let renewal_message_id = message_id.to_owned();
        let renewal_owner = owner.clone();
        let renewal = tokio::spawn(async move {
            let interval = Duration::from_secs(
                u64::try_from(MESSAGE_CONTENT_FETCH_LEASE_SECONDS / 3).unwrap_or(30),
            );
            loop {
                tokio::time::sleep(interval).await;
                let renewed = sqlx::query(
                    "UPDATE message_content_fetches SET claimed_at = ? WHERE message_id = ? AND claim_owner = ?",
                )
                .bind(Utc::now())
                .bind(&renewal_message_id)
                .bind(&renewal_owner)
                .execute(&renewal_store.pool)
                .await;
                if !matches!(renewed, Ok(result) if result.rows_affected() == 1) {
                    break;
                }
            }
        });
        Ok(Some(MessageContentFetchClaim {
            store: self.clone(),
            message_id: message_id.to_owned(),
            owner,
            renewal,
            released: false,
        }))
    }

    /// Acquires a fetch claim while preserving why it was unavailable.
    ///
    /// Background work only needs a best-effort claim, but foreground opens
    /// must fail a stale/deleted message immediately instead of polling it as
    /// though another fetch still owned it.
    pub async fn acquire_message_content_fetch_outcome(
        &self,
        message_id: &str,
    ) -> Result<MessageContentFetchAcquire> {
        if let Some(claim) = self.acquire_message_content_fetch(message_id).await? {
            return Ok(MessageContentFetchAcquire::Claimed(claim));
        }

        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?)")
            .bind(message_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(if exists {
            MessageContentFetchAcquire::Busy
        } else {
            MessageContentFetchAcquire::Missing
        })
    }

    pub async fn release_message_content_fetch(&self, message_id: &str) -> Result<()> {
        self.release_message_content_fetch_for_owner(message_id, message_content_fetch_owner())
            .await
    }

    async fn release_message_content_fetch_for_owner(
        &self,
        message_id: &str,
        owner: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM message_content_fetches WHERE message_id = ? AND claim_owner = ?")
            .bind(message_id)
            .bind(owner)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn starred_conversation_count(&self, account_ids: &[AccountId]) -> Result<u64> {
        if account_ids.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; account_ids.len()].join(",");
        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT account_id, thread_id FROM messages WHERE is_flagged = 1 AND mailbox NOT IN ('Spam', 'Trash') AND mailbox NOT LIKE 'Spam::%' AND mailbox NOT LIKE 'Trash::%' AND account_id IN ({placeholders}) GROUP BY account_id, thread_id)"
        );
        let mut statement = sqlx::query_scalar::<_, i64>(&sql);
        for account_id in account_ids {
            statement = statement.bind(account_id.to_string());
        }
        Ok(statement.fetch_one(&self.pool).await?.max(0) as u64)
    }

    pub async fn incomplete_inbox_messages(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND mailbox = 'INBOX' AND content_state != 'complete' ORDER BY received_at DESC LIMIT ?";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(account_id.to_string())
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn uncached_starred_messages(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals FROM messages m LEFT JOIN starred_message_bodies b ON b.message_id = m.id WHERE m.account_id = ? AND m.is_flagged = 1 AND (b.message_id IS NULL OR b.attachment_presentation_version != ?) ORDER BY m.received_at DESC LIMIT ?";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(account_id.to_string())
            .bind(ATTACHMENT_PRESENTATION_VERSION)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn unscanned_mailbox_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT uid FROM messages WHERE account_id = ? AND mailbox = ? AND unsubscribe_scanned = 0 ORDER BY received_at DESC LIMIT ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid,)| u32::try_from(uid).ok())
            .collect())
    }

    pub async fn unscanned_threading_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT uid FROM messages WHERE account_id = ? AND mailbox = ? AND threading_scanned = 0 ORDER BY received_at DESC, uid DESC LIMIT ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid,)| u32::try_from(uid).ok())
            .collect())
    }

    /// Returns a bounded, newest-first recipient-header upgrade batch. Rows
    /// are marked complete only after their actual provider headers are
    /// saved, including the truthful case where Cc, Bcc, and Reply-To are all
    /// absent.
    pub async fn unscanned_recipient_header_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT uid FROM messages WHERE account_id = ? AND mailbox = ? AND recipient_headers_scanned = 0 ORDER BY received_at DESC, uid DESC LIMIT ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(limit.clamp(1, 100) as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid,)| u32::try_from(uid).ok())
            .collect())
    }

    pub async fn save_recipient_headers(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
        cc_addresses: &str,
        bcc_addresses: &str,
        reply_to_addresses: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE messages SET cc_addresses = ?, bcc_addresses = ?, reply_to_addresses = ?, recipient_headers_scanned = 1 WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(cc_addresses)
            .bind(bcc_addresses)
            .bind(reply_to_addresses)
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(uid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_threading_headers(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
        headers: &ThreadingHeaders,
    ) -> Result<()> {
        sqlx::query("UPDATE messages SET message_id = COALESCE(?, message_id), in_reply_to = COALESCE(?, in_reply_to), reference_ids = COALESCE(?, reference_ids), threading_scanned = 1 WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(&headers.message_id)
            .bind(&headers.in_reply_to)
            .bind(&headers.reference_ids)
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(uid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn finish_threading_backfill(&self, account_id: AccountId) -> Result<()> {
        self.rebuild_threads_for_account(&account_id.to_string())
            .await
    }

    /// Returns messages whose timestamp came from the old `Utc::now()`
    /// fallback. RFC message dates are parsed at whole-second precision, while
    /// that fallback was stored with a fractional second. Refetching these
    /// rows lets sync replace the invented value with IMAP INTERNALDATE.
    pub async fn legacy_assumed_date_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT uid FROM messages WHERE account_id = ? AND mailbox = ? AND instr(received_at, '.') > 0 ORDER BY received_at DESC LIMIT ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid,)| u32::try_from(uid).ok())
            .collect())
    }

    /// Returns catalogue rows created by the old partial-body preview path,
    /// which stored base64-encoded HTML as if it were readable text.
    pub async fn mime_encoded_snippet_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT uid, snippet FROM messages WHERE account_id = ? AND mailbox = ? AND trim(body_text) = '' AND trim(snippet) != '' ORDER BY received_at DESC, uid DESC",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter(|(_, snippet)| {
                STANDARD
                    .decode(snippet)
                    .ok()
                    .and_then(|decoded| String::from_utf8(decoded).ok())
                    .is_some_and(|decoded| decoded.trim_start().starts_with('<'))
            })
            .filter_map(|(uid, _)| u32::try_from(uid).ok())
            .collect())
    }

    /// Returns rows affected by the old MIME attachment predicate, which
    /// treated unnamed inline text body parts as downloadable attachments.
    /// A successful refetch removes those bogus attachment rows, so repaired
    /// messages stop matching this query without a separate migration flag.
    pub async fn misclassified_body_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT m.uid FROM messages m WHERE m.account_id = ? AND m.mailbox = ? AND trim(m.body_text) = '' AND (m.body_html IS NULL OR trim(m.body_html) = '') AND EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id AND a.is_inline = 1 AND a.filename = 'attachment' AND a.mime_type IN ('text/plain', 'text/html')) ORDER BY m.received_at DESC, m.uid DESC LIMIT ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid,)| u32::try_from(uid).ok())
            .collect())
    }

    pub async fn move_message(
        &self,
        account_id: AccountId,
        source_mailbox: &str,
        source_uid: u32,
        destination_mailbox: &str,
        destination_uid: Option<u32>,
    ) -> Result<()> {
        let account_key = account_id.to_string();
        let mut tx = self.pool.begin().await?;
        // A realtime fetch may have read this UID immediately before the
        // provider completed the move. Record the successful action in the
        // same transaction as the local move so that late sync publication
        // cannot resurrect a locator that no longer exists remotely.
        sqlx::query("INSERT OR REPLACE INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, ?, ?, ?)")
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        let Some(destination_uid) = destination_uid else {
            sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(&account_key)
                .bind(source_mailbox)
                .bind(source_uid)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        };
        sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(&account_key)
            .bind(destination_mailbox)
            .bind(destination_uid)
            .execute(&mut *tx)
            .await?;
        // Attachment identifiers encode the message locator. Invalidate the
        // source cache instead of cascading it to the destination locator.
        sqlx::query("DELETE FROM message_content_cache WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .execute(&mut *tx)
            .await?;
        // Starred attachment metadata uses the same opaque IDs as the
        // foreground cache. Do not cascade those IDs to the new locator:
        // a later targeted download would resolve the old message prefix
        // against the destination's MIME structure. Removing the durable
        // starred body also makes the destination eligible for an
        // authoritative refetch.
        sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .execute(&mut *tx)
            .await?;
        // In-flight fetch ownership is tied to the old locator. Revoke it
        // before the message ID update instead of allowing the FK cascade to
        // move a claim that its owner can only release by the old ID.
        sqlx::query("DELETE FROM message_content_fetches WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE messages SET id = ?, mailbox = ?, uid = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(stable_message_id(account_id, destination_mailbox, destination_uid))
            .bind(destination_mailbox)
            .bind(destination_uid)
            .bind(&account_key)
            .bind(source_mailbox)
            .bind(source_uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reconciles one provider move that represented several local catalogue
    /// locators. The whole operation is one transaction so a missing duplicate
    /// cannot delete a Trash row created from an earlier existing source.
    pub async fn move_messages_to_destination(
        &self,
        account_id: AccountId,
        sources: &[(String, u32)],
        destination_mailbox: &str,
        destination_uid: Option<u32>,
    ) -> Result<()> {
        let account_key = account_id.to_string();
        let mut unique_sources = Vec::new();
        let mut seen = HashSet::new();
        for (mailbox, uid) in sources {
            if seen.insert((mailbox.clone(), *uid)) {
                unique_sources.push((mailbox, *uid));
            }
        }
        if unique_sources.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (mailbox, uid) in &unique_sources {
            sqlx::query("INSERT OR REPLACE INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, ?, ?, ?)")
                .bind(&account_key)
                .bind(mailbox)
                .bind(uid)
                .bind(Utc::now())
                .execute(&mut *tx)
                .await?;
        }
        let Some(destination_uid) = destination_uid else {
            for (mailbox, uid) in &unique_sources {
                sqlx::query(
                    "DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
                )
                .bind(&account_key)
                .bind(mailbox)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(());
        };

        let mut chosen = None;
        for (mailbox, uid) in &unique_sources {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)",
            )
            .bind(&account_key)
            .bind(mailbox)
            .bind(uid)
            .fetch_one(&mut *tx)
            .await?;
            if exists && chosen.is_none() {
                chosen = Some(((*mailbox).clone(), *uid));
            }
        }
        let Some((chosen_mailbox, chosen_uid)) = chosen else {
            tx.commit().await?;
            return Ok(());
        };

        for (mailbox, uid) in &unique_sources {
            for table in [
                "message_content_cache",
                "starred_attachment_metadata",
                "starred_message_bodies",
                "message_content_fetches",
            ] {
                let query = format!(
                    "DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)"
                );
                sqlx::query(&query)
                    .bind(&account_key)
                    .bind(mailbox)
                    .bind(uid)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // Delete a pre-existing destination only after we know an existing
        // source will replace it. This is what protects a destination from a
        // later absent alias in the same provider move.
        sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(&account_key)
            .bind(destination_mailbox)
            .bind(destination_uid)
            .execute(&mut *tx)
            .await?;
        for (mailbox, uid) in &unique_sources {
            if mailbox.as_str() != chosen_mailbox || *uid != chosen_uid {
                sqlx::query(
                    "DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
                )
                .bind(&account_key)
                .bind(mailbox)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query("UPDATE messages SET id = ?, mailbox = ?, uid = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(stable_message_id(account_id, destination_mailbox, destination_uid))
            .bind(destination_mailbox)
            .bind(destination_uid)
            .bind(&account_key)
            .bind(&chosen_mailbox)
            .bind(chosen_uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<MailSummary>> {
        self.search_with_projection(
            query,
            "m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals",
        )
        .await
    }

    async fn search_with_projection(
        &self,
        query: &SearchQuery,
        _projection: &str,
    ) -> Result<Vec<MailSummary>> {
        let limit = query.limit.unwrap_or(100).clamp(1, 500) as i64;
        let expression = parse_search_query(&query.text)?;
        let mut rows = self.search_matching_messages(query, &expression).await?;
        if let Some(cursor) = &query.cursor {
            rows.retain(|message| {
                message.received_at < cursor.received_at
                    || (message.received_at == cursor.received_at && message.id < cursor.id)
            });
        }
        rows.truncate(limit as usize);
        Ok(rows)
    }

    /// Finds conversations by messages matching the requested view, then
    /// hydrates their allowed account-wide members. This intentionally keeps
    /// mailbox membership separate from reader membership.
    pub async fn search_conversations(&self, query: &SearchQuery) -> Result<Vec<MailConversation>> {
        Ok(self.search_conversation_page(query).await?.conversations)
    }

    /// Re-evaluates the canonical local expression for selected persisted
    /// conversation keys and returns exact evidence from those matching rows.
    /// Provider and Tauri callers must pass local account/thread IDs, never a
    /// provider UID, and must not derive evidence from list-hydrated bodies.
    pub async fn search_match_evidence_for_conversations(
        &self,
        query: &SearchQuery,
        conversation_keys: &[(String, String)],
    ) -> Result<BTreeMap<String, SearchMatchEvidence>> {
        if conversation_keys.is_empty() {
            return Ok(BTreeMap::new());
        }
        let expression = parse_search_query(&query.text)?;
        let matches = self
            .search_matching_messages_for_threads(query, &expression, conversation_keys)
            .await?;
        Ok(matches
            .into_iter()
            .map(|((account_id, thread_id), matches)| {
                (
                    format!("{account_id}:{thread_id}"),
                    search_match_evidence(&matches),
                )
            })
            .collect())
    }

    async fn search_matching_messages_for_threads(
        &self,
        query: &SearchQuery,
        expression: &SearchExpression,
        conversation_keys: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<MailSummary>>> {
        let mut matches: HashMap<(String, String), Vec<MailSummary>> = HashMap::new();
        let mut cursor = None;
        loop {
            let chunk = self
                .search_matching_message_chunk(
                    query,
                    expression,
                    cursor.as_ref(),
                    Some(conversation_keys),
                    SEARCH_CANDIDATE_SCAN_CHUNK,
                )
                .await?;
            let last_cursor = chunk.rows.last().map(search_message_cursor);
            for (message, matched) in chunk.rows.into_iter().zip(chunk.matches) {
                if matched {
                    let key = (message.account_id.clone(), message.thread_id.clone());
                    matches.entry(key).or_default().push(message);
                }
            }
            if chunk.exhausted || last_cursor.is_none() {
                break;
            }
            cursor = last_cursor;
        }
        Ok(matches)
    }

    /// Fetches the local search corpus once, then evaluates the parsed AST
    /// against the same projection for flat and conversation search. SQLite
    /// retains the account, mailbox, and legacy-filter predicates so the
    /// evaluator never crosses an account or view boundary; text evaluation
    /// stays in Rust because it is the canonical Unicode/phrase/prefix
    /// implementation shared with provider verification.
    async fn search_matching_messages(
        &self,
        query: &SearchQuery,
        expression: &SearchExpression,
    ) -> Result<Vec<MailSummary>> {
        let mut cursor = None;
        let mut matches = Vec::new();
        loop {
            let chunk = self
                .search_matching_message_chunk(
                    query,
                    expression,
                    cursor.as_ref(),
                    None,
                    SEARCH_CANDIDATE_SCAN_CHUNK,
                )
                .await?;
            let last_cursor = chunk.rows.last().map(search_message_cursor);
            matches.extend(
                chunk
                    .rows
                    .into_iter()
                    .zip(chunk.matches)
                    .filter_map(|(row, matched)| matched.then_some(row)),
            );
            if chunk.exhausted {
                return Ok(matches);
            }
            cursor = last_cursor;
            // A non-exhausted keyset batch always contains at least one row.
            // Keep this defensive return so a malformed database response
            // cannot spin a draft preview forever.
            if cursor.is_none() {
                return Ok(matches);
            }
        }
    }

    /// Fetch one keyset-bounded candidate batch and evaluate it canonically.
    /// `thread_keys` is used for exact evidence after a conversation page has
    /// chosen its representatives, so old matching messages in that thread
    /// remain visible without rescanning the entire mailbox.
    async fn search_matching_message_chunk(
        &self,
        query: &SearchQuery,
        expression: &SearchExpression,
        cursor: Option<&MailCursor>,
        thread_keys: Option<&[(String, String)]>,
        limit: usize,
    ) -> Result<SearchCandidateChunk> {
        // The compiler only narrows SQLite candidates. It intentionally
        // broadens branches it cannot prove safe (notably partial OR/NOT
        // branches), and the canonical evaluator below remains authoritative.
        let today = Utc::now().date_naive();
        let catalogue_complete = self.search_catalogue_v2_complete().await?;
        let candidate =
            if !catalogue_complete || expression_contains_no_attachment(&expression.root) {
                // `has:noattachment` is evaluated over user-facing attachments.
                // A catalogue row for an inline CID image must therefore not
                // remove its message from the candidate set before the canonical
                // evaluator has a chance to apply that distinction. A partial
                // restart-safe v2 migration has the same requirement: its FTS
                // and attachment catalogue cannot omit older canonical matches.
                SqlSearchCandidate {
                    predicate: "1 = 1".into(),
                    binds: Vec::new(),
                    requires_post_filter: true,
                }
            } else {
                compile_sql_candidate(expression, today)?
            };
        let projection = "m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, COALESCE(s.body_text, c.body_text, b.body_text, m.body_text) AS body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals";
        let mut sql = format!("SELECT {projection} FROM messages m LEFT JOIN message_search_body_text s ON s.message_id = m.id LEFT JOIN message_content_cache c ON c.message_id = m.id LEFT JOIN starred_message_bodies b ON b.message_id = m.id WHERE 1=1");
        if !query.account_ids.is_empty() {
            sql.push_str(" AND m.account_id IN (");
            sql.push_str(&vec!["?"; query.account_ids.len()].join(","));
            sql.push(')');
        }
        if let Some(mailbox) = query.mailbox.as_deref() {
            if is_special_mailbox_family(mailbox) {
                sql.push_str(" AND (m.mailbox = ? OR m.mailbox LIKE ?)");
            } else {
                sql.push_str(" AND m.mailbox = ?");
            }
        } else if positive_folder_scopes(&expression.root).is_empty() {
            sql.push_str(" AND m.mailbox NOT IN ('Spam', 'Trash') AND m.mailbox NOT LIKE 'Spam::%' AND m.mailbox NOT LIKE 'Trash::%'");
        }
        if query.from.is_some() {
            sql.push_str(" AND m.from_address LIKE ?");
        }
        if query.unread_only {
            sql.push_str(" AND m.is_read = 0");
        }
        if query.read_only {
            sql.push_str(" AND m.is_read = 1");
        }
        if query.flagged_only {
            sql.push_str(" AND m.is_flagged = 1");
        }
        if query.unflagged_only {
            sql.push_str(" AND NOT EXISTS (SELECT 1 FROM messages flagged WHERE flagged.account_id = m.account_id AND flagged.thread_id = m.thread_id AND flagged.is_flagged = 1)");
        }
        if query.category.is_some() {
            sql.push_str(" AND m.category = ?");
        }
        if let Some(thread_keys) = thread_keys {
            if thread_keys.is_empty() {
                return Ok(SearchCandidateChunk {
                    rows: Vec::new(),
                    matches: Vec::new(),
                    exhausted: true,
                });
            }
            sql.push_str(" AND (");
            sql.push_str(
                &vec!["(m.account_id = ? AND m.thread_id = ?)"; thread_keys.len()].join(" OR "),
            );
            sql.push(')');
        }
        sql.push_str(" AND (");
        sql.push_str(&candidate.predicate);
        sql.push(')');
        if cursor.is_some() {
            sql.push_str(" AND (m.received_at < ? OR (m.received_at = ? AND m.id < ?))");
        }
        sql.push_str(" ORDER BY m.received_at DESC, m.id DESC");
        sql.push_str(" LIMIT ?");

        let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
        for account_id in &query.account_ids {
            statement = statement.bind(account_id.to_string());
        }
        if let Some(mailbox) = query.mailbox.as_deref() {
            statement = statement.bind(mailbox);
            if is_special_mailbox_family(mailbox) {
                statement = statement.bind(format!("{mailbox}::%"));
            }
        }
        if let Some(from) = &query.from {
            statement = statement.bind(format!("%{from}%"));
        }
        if let Some(category) = &query.category {
            statement = statement.bind(category);
        }
        if let Some(thread_keys) = thread_keys {
            for (account_id, thread_id) in thread_keys {
                statement = statement.bind(account_id).bind(thread_id);
            }
        }
        for bind in candidate.binds {
            statement = match bind {
                SqlSearchBind::Text(value) => statement.bind(value),
                SqlSearchBind::Integer(value) => statement.bind(value),
            };
        }
        if let Some(cursor) = cursor {
            statement = statement
                .bind(cursor.received_at)
                .bind(cursor.received_at)
                .bind(&cursor.id);
        }
        statement = statement.bind(limit.saturating_add(1) as i64);
        let rows = statement.fetch_all(&self.pool).await?;
        let exhausted = rows.len() <= limit;
        let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(SearchCandidateChunk {
                rows,
                matches: Vec::new(),
                exhausted: true,
            });
        }

        let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let attachments = self.search_attachments_by_message_id(&ids).await?;
        let mailbox_memberships = self.search_mailbox_paths_by_message_id(&ids).await?;
        let matches = rows
            .iter()
            .map(|row| {
                let from = row
                    .from_name
                    .as_deref()
                    .map(|name| format!("{name} <{}>", row.from_address))
                    .unwrap_or_else(|| row.from_address.clone());
                let attachment_rows = attachments.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
                let mut searchable_attachments = attachment_rows
                    .iter()
                    .map(|(filename, mime_type)| SearchableAttachment {
                        filename: filename.as_deref(),
                        mime_type: mime_type.as_deref(),
                    })
                    .collect::<Vec<_>>();
                // Older catalogued messages can know that an attachment
                // exists before their MIME metadata is refreshed. Preserve
                // correct has/no-attachment semantics without inventing a
                // filename or type for them.
                if !attachments.contains_key(&row.id) && row.has_attachments {
                    searchable_attachments.push(SearchableAttachment::default());
                }
                // A message can have one physical row per provider mailbox.
                // Folder evaluation is logical, so expose every catalogue
                // local path from same-account RFC Message-ID aliases while
                // retaining the row's legacy storage mailbox as a fallback
                // for profiles that have not received membership data yet.
                let mut searchable_mailboxes = mailbox_memberships
                    .get(&row.id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                searchable_mailboxes.push(row.mailbox.as_str());
                let body = if row.body_text.is_empty() {
                    row.snippet.as_str()
                } else {
                    row.body_text.as_str()
                };
                evaluate_search(
                    expression,
                    &SearchableMessage {
                        from: &from,
                        to: &row.to_addresses,
                        cc: &row.cc_addresses,
                        bcc: &row.bcc_addresses,
                        subject: &row.subject,
                        body,
                        mailbox: &row.mailbox,
                        mailboxes: &searchable_mailboxes,
                        received_on: row.received_at.date_naive(),
                        attachments: &searchable_attachments,
                        is_read: row.is_read,
                        is_flagged: row.is_flagged,
                        is_replied: row.is_answered,
                        is_draft: row.is_draft,
                    },
                    today,
                )
            })
            .collect();
        Ok(SearchCandidateChunk {
            rows,
            matches,
            exhausted,
        })
    }

    async fn search_attachments_by_message_id(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<SearchAttachment>>> {
        let mut attachments: HashMap<String, Vec<SearchAttachment>> = HashMap::new();
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT message_id, filename, mime_type, is_inline, presentation FROM message_attachment_catalogue WHERE message_id IN ({placeholders})");
            let mut statement =
                sqlx::query_as::<_, (String, String, String, bool, AttachmentPresentation)>(&sql);
            for id in chunk {
                statement = statement.bind(id);
            }
            let rows = statement.fetch_all(&self.pool).await?;
            for (message_id, filename, mime_type, is_inline, presentation) in rows {
                let message_attachments = attachments.entry(message_id).or_default();
                match presentation {
                    // A CID part can be both shown in HTML and deliberately
                    // offered as a file. The provider's canonical matching
                    // uses this same distinction.
                    AttachmentPresentation::Downloadable | AttachmentPresentation::Both => {
                        message_attachments.push((Some(filename), Some(mime_type)));
                    }
                    // An embedded-only resource is never a user-facing
                    // attachment, even if older headers still carry a broad
                    // `has_attachments` bit.
                    AttachmentPresentation::Embedded => {}
                    AttachmentPresentation::Unknown if !is_inline => {
                        // Legacy rows have only the transport disposition. A
                        // non-inline part can establish has/no-attachment
                        // state, but its filename and MIME type are not
                        // trusted for filename/filetype matching.
                        message_attachments.push((None, None));
                    }
                    AttachmentPresentation::Unknown => {}
                }
            }
        }
        Ok(attachments)
    }

    /// Loads the user-visible local paths for every logical message selected
    /// as a search candidate. A provider may expose the same RFC message in
    /// more than one physical mailbox row, and a folder query must see the
    /// union of their memberships. The direct-ID branch also covers messages
    /// without a valid RFC Message-ID.
    async fn search_mailbox_paths_by_message_id(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut memberships: HashMap<String, Vec<String>> = HashMap::new();
        if ids.is_empty()
            || !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM message_mailbox_memberships LIMIT 1)",
            )
            .fetch_one(&self.pool)
            .await?
        {
            // Legacy and ordinary catalogues can rely entirely on the
            // message row's mailbox fallback. Avoid joining every candidate
            // back through the messages table when no logical memberships
            // exist anywhere yet. Besides saving work during migration, this
            // keeps broad local previews bounded while their mailbox
            // catalogue is still being built.
            return Ok(memberships);
        }
        for chunk in ids.chunks(300) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT candidate.id, mailbox.local_path \
                 FROM messages candidate \
                 JOIN messages logical ON logical.account_id = candidate.account_id \
                 JOIN message_mailbox_memberships membership \
                   ON membership.message_id = logical.id \
                  AND membership.account_id = candidate.account_id \
                 JOIN selectable_mailboxes mailbox \
                   ON mailbox.id = membership.mailbox_id \
                  AND mailbox.account_id = membership.account_id \
                 WHERE candidate.id IN ({placeholders}) \
                   AND (logical.id = candidate.id OR (candidate.message_id IS NOT NULL \
                        AND logical.message_id IS NOT NULL \
                        AND lower(trim(logical.message_id)) = lower(trim(candidate.message_id)))) \
                 ORDER BY candidate.id, mailbox.local_path, mailbox.id"
            );
            let mut statement = sqlx::query_as::<_, (String, String)>(&sql);
            for id in chunk {
                statement = statement.bind(id);
            }
            for (message_id, local_path) in statement.fetch_all(&self.pool).await? {
                let paths = memberships.entry(message_id).or_default();
                if !paths.contains(&local_path) {
                    paths.push(local_path);
                }
            }
        }
        Ok(memberships)
    }

    /// Returns every user-visible local mailbox path for each supplied local
    /// message ID. Paths include memberships persisted on same-account
    /// physical aliases with the same canonical RFC Message-ID, so provider
    /// pages can apply the same logical folder semantics as local search.
    pub async fn logical_mailbox_paths_by_message_ids(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        self.search_mailbox_paths_by_message_id(ids).await
    }

    /// Resolves a notification or deep-link target without applying list
    /// pagination. A concrete local ID wins while it remains valid; a moved
    /// message can fall back to its RFC Message-ID, then its supplied thread
    /// ID. A local ID found under another account, or an RFC Message-ID that
    /// spans multiple threads, is never allowed to select a conversation.
    pub async fn conversation_for_target(
        &self,
        target: &ConversationTarget,
    ) -> Result<Option<MailConversation>> {
        let account_id = target.account_id.to_string();
        if let Some(local_message_id) = target
            .local_message_id
            .as_deref()
            .filter(|message_id| !message_id.trim().is_empty())
        {
            if let Some(message) = self.message(local_message_id).await? {
                if message.account_id != account_id || !target_allows_mailbox(&message, target) {
                    return Ok(None);
                }
                let rfc_matches = target
                    .rfc_message_id
                    .as_deref()
                    .and_then(normalize_message_id)
                    .is_none_or(|expected| {
                        message
                            .message_id
                            .as_deref()
                            .and_then(normalize_message_id)
                            .as_deref()
                            == Some(expected.as_str())
                    });
                let thread_matches = target
                    .thread_id
                    .as_deref()
                    .filter(|thread_id| !thread_id.trim().is_empty())
                    .is_none_or(|thread_id| message.thread_id == thread_id);
                if rfc_matches && thread_matches {
                    return self
                        .complete_conversation(
                            &message.account_id,
                            &message.thread_id,
                            target.mailbox.as_deref(),
                        )
                        .await;
                }
            }
        }

        if let Some(rfc_message_id) = target
            .rfc_message_id
            .as_deref()
            .and_then(normalize_message_id)
        {
            let candidates = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND LOWER(TRIM(message_id)) = ?")
                .bind(&account_id)
                .bind(format!("<{rfc_message_id}>"))
                .fetch_all(&self.pool)
                .await?;
            let thread_ids = candidates
                .into_iter()
                .filter(|message| target_allows_mailbox(message, target))
                .filter(|message| {
                    message
                        .message_id
                        .as_deref()
                        .and_then(normalize_message_id)
                        .as_deref()
                        == Some(rfc_message_id.as_str())
                })
                .map(|message| message.thread_id)
                .collect::<HashSet<_>>();
            match thread_ids.len() {
                0 => {}
                1 => {
                    let thread_id = thread_ids
                        .into_iter()
                        .next()
                        .expect("one RFC Message-ID match has one thread");
                    return self
                        .complete_conversation(&account_id, &thread_id, target.mailbox.as_deref())
                        .await;
                }
                _ => return Ok(None),
            }
        }

        if let Some(thread_id) = target
            .thread_id
            .as_deref()
            .filter(|thread_id| !thread_id.trim().is_empty())
        {
            let exists = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND thread_id = ? ORDER BY received_at, id")
                .bind(&account_id)
                .bind(thread_id)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .any(|message| target_allows_mailbox(&message, target));
            if exists {
                return self
                    .complete_conversation(&account_id, thread_id, target.mailbox.as_deref())
                    .await;
            }
        }

        Ok(None)
    }

    async fn complete_conversation(
        &self,
        account_id: &str,
        thread_id: &str,
        mailbox: Option<&str>,
    ) -> Result<Option<MailConversation>> {
        let mut source_messages = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, '' AS body_text, NULL AS body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, '' AS classification_signals FROM messages WHERE account_id = ? AND thread_id = ? ORDER BY received_at, id")
            .bind(account_id)
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await?;
        if mailbox.is_some_and(|mailbox| matches!(mailbox_family(mailbox), "Spam" | "Trash")) {
            let mailbox = mailbox_family(mailbox.unwrap_or_default());
            source_messages.retain(|message| mailbox_family(&message.mailbox) == mailbox);
        } else {
            source_messages
                .retain(|message| !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"));
        }
        Ok(mail_conversation_from_sources(
            account_id.to_owned(),
            thread_id.to_owned(),
            source_messages,
            mailbox,
        ))
    }

    /// Returns a page of matching conversations. The cursor addresses the
    /// newest matching message for each conversation, not its hydrated
    /// account-wide members, so a thread can never reappear on a later page.
    pub async fn search_conversation_page(
        &self,
        query: &SearchQuery,
    ) -> Result<MailConversationPage> {
        self.search_conversation_page_from_candidate(query, None, &[])
            .await
    }

    /// Session callers resume this path with an opaque candidate cursor and
    /// the conversations already emitted by that session.  The result cursor
    /// remains available for legacy callers, but is intentionally separate
    /// from candidate progress so broad predicates cannot rescan the whole
    /// corpus on every V2 page.
    pub async fn search_conversation_page_from_candidate(
        &self,
        query: &SearchQuery,
        candidate_cursor: Option<&MailCursor>,
        excluded_conversation_ids: &[String],
    ) -> Result<MailConversationPage> {
        let expression = parse_search_query(&query.text)?;
        let positive_system_scopes = positive_folder_scopes(&expression.root);
        let include_system_mailboxes = query
            .mailbox
            .as_deref()
            .is_some_and(is_special_mailbox_family)
            || !positive_system_scopes.is_empty();
        let limit = query.limit.unwrap_or(100).clamp(1, 500);
        let mut match_query = query.clone();
        // Select one representative matching message per conversation before
        // paging. Paging raw messages and grouping afterwards makes a thread
        // straddle pages, causing duplicates and unreliable `hasMore`.
        match_query.limit = Some(limit.saturating_add(1));
        let mut matching = self
            .search_conversation_matches_from_candidate(
                &match_query,
                candidate_cursor,
                excluded_conversation_ids,
            )
            .await?;
        let has_more = matching.representatives.len() > limit as usize;
        matching.representatives.truncate(limit as usize);
        let next_cursor = has_more.then(|| {
            let last = matching
                .representatives
                .last()
                .expect("a page with more results contains a cursor source");
            MailCursor {
                received_at: last.received_at,
                id: last.id.clone(),
            }
        });
        // The lookahead row is intentionally not consumed. Continuing from
        // the final returned representative may revisit a few non-matching
        // candidates, but it cannot skip the lookahead conversation and the
        // session's emitted-ID set prevents overlap.
        let next_candidate_cursor = has_more.then(|| {
            matching
                .representatives
                .last()
                .map(search_message_cursor)
                .expect("a page with a lookahead has a returned representative")
        });
        let isolated = query
            .mailbox
            .as_deref()
            .is_some_and(|mailbox| matches!(mailbox, "Spam" | "Trash"));
        if !include_system_mailboxes {
            matching
                .representatives
                .retain(|message| !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"));
        }

        let keys = matching
            .representatives
            .iter()
            .map(|message| (message.account_id.clone(), message.thread_id.clone()))
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(MailConversationPage {
                conversations: Vec::new(),
                match_evidence: BTreeMap::new(),
                next_cursor: None,
                candidate_cursor: if matching.exhausted {
                    None
                } else {
                    next_candidate_cursor.or(matching.candidate_cursor)
                },
                candidate_exhausted: matching.exhausted,
            });
        }

        let mut hydrated = Vec::new();
        let matching_message_ids = matching
            .representatives
            .iter()
            .map(|message| message.id.as_str())
            .collect::<HashSet<_>>();
        // Keep well below SQLite's conservative parameter limit while avoiding
        // one hydration query per conversation.
        for chunk in keys.chunks(300) {
            let predicates = vec!["(account_id = ? AND thread_id = ?)"; chunk.len()].join(" OR ");
            let sql = format!("SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, '' AS body_text, NULL AS body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, '' AS classification_signals FROM messages m WHERE ({predicates}) ORDER BY m.received_at, m.id");
            let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
            for (account_id, thread_id) in chunk {
                statement = statement.bind(account_id).bind(thread_id);
            }
            hydrated.extend(statement.fetch_all(&self.pool).await?);
        }
        if isolated {
            let mailbox = query.mailbox.as_deref().unwrap_or_default();
            hydrated.retain(|message| mailbox_family(&message.mailbox) == mailbox);
        } else {
            hydrated.retain(|message| {
                !is_system_mailbox(&message.mailbox)
                    // A logical label can live on a physical Spam/Trash row.
                    // Keep the already canonically matched representative even
                    // when its physical storage mailbox cannot itself explain
                    // the positive folder scope.
                    || matching_message_ids.contains(message.id.as_str())
                    || positive_system_scopes
                        .iter()
                        .any(|scope| folder_scope_matches_system_mailbox(scope, &message.mailbox))
            });
        }

        let mut grouped: HashMap<(String, String), Vec<MailSummary>> = HashMap::new();
        for message in hydrated {
            grouped
                .entry((message.account_id.clone(), message.thread_id.clone()))
                .or_default()
                .push(message);
        }
        let conversations = grouped
            .into_iter()
            .filter_map(|((account_id, thread_id), source_messages)| {
                mail_conversation_from_sources(
                    account_id,
                    thread_id,
                    source_messages,
                    query.mailbox.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        let conversations = conversations
            .into_iter()
            .map(|conversation| {
                (
                    (
                        conversation.account_id.clone(),
                        conversation.thread_id.clone(),
                    ),
                    conversation,
                )
            })
            .collect::<HashMap<_, _>>();
        let conversations = keys
            .into_iter()
            .filter_map(|key| conversations.get(&key).cloned())
            .collect::<Vec<_>>();
        let match_evidence = conversations
            .iter()
            .filter_map(|conversation| {
                let key = (
                    conversation.account_id.clone(),
                    conversation.thread_id.clone(),
                );
                matching
                    .matched_by_thread
                    .get(&key)
                    .map(|matches| (conversation.id.clone(), search_match_evidence(matches)))
            })
            .collect();
        Ok(MailConversationPage {
            conversations,
            match_evidence,
            next_cursor,
            candidate_cursor: if matching.exhausted {
                None
            } else {
                next_candidate_cursor.or(matching.candidate_cursor)
            },
            candidate_exhausted: matching.exhausted,
        })
    }

    /// Loads the initial Smart Inbox sections in one SQLite statement and
    /// hydrates the union of their selected conversations once. Section
    /// pagination continues through `search_conversation_page` so cursors keep
    /// the same public meaning after the initial page.
    pub async fn search_smart_inbox(&self, query: &SmartInboxQuery) -> Result<SmartInboxPage> {
        const SECTION_IDS: [&str; 7] = [
            "starred",
            "people",
            "transactions",
            "notifications",
            "newsletters",
            "other",
            "seen",
        ];
        let limit = query.limit.unwrap_or(3).clamp(1, 100) as usize;
        if query.account_ids.is_empty() {
            return Ok(SmartInboxPage {
                sections: SECTION_IDS
                    .into_iter()
                    .map(|id| SmartInboxSectionPage {
                        id: id.to_owned(),
                        conversations: Vec::new(),
                        next_cursor: None,
                    })
                    .collect(),
            });
        }

        let account_placeholders = vec!["?"; query.account_ids.len()].join(",");
        let sql = format!(
            r#"WITH scoped AS (
                SELECT m.id, m.account_id, m.thread_id, m.received_at, m.category,
                    ROW_NUMBER() OVER (PARTITION BY m.account_id, m.thread_id ORDER BY m.received_at DESC, m.id DESC) AS thread_rank,
                    MAX(CASE WHEN m.is_read = 0 THEN 1 ELSE 0 END) OVER (PARTITION BY m.account_id, m.thread_id) AS any_unread
                FROM messages m
                WHERE m.account_id IN ({account_placeholders}) AND m.mailbox = 'INBOX'
            ), thread_flags AS (
                SELECT account_id, thread_id, 1 AS any_flagged
                FROM messages
                WHERE account_id IN ({account_placeholders}) AND is_flagged = 1
                GROUP BY account_id, thread_id
            ), representatives AS (
                SELECT scoped.id, scoped.account_id, scoped.thread_id, scoped.received_at, scoped.category, scoped.any_unread,
                    COALESCE(thread_flags.any_flagged, 0) AS any_flagged
                FROM scoped
                LEFT JOIN thread_flags USING (account_id, thread_id)
                WHERE scoped.thread_rank = 1
            ), sectioned AS (
                SELECT 'starred' AS section_id, id, account_id, thread_id, received_at FROM representatives WHERE any_flagged = 1
                UNION ALL
                SELECT category AS section_id, id, account_id, thread_id, received_at FROM representatives
                    WHERE any_flagged = 0 AND any_unread = 1 AND category IN ('people', 'transactions', 'notifications', 'newsletters', 'other')
                UNION ALL
                SELECT 'seen' AS section_id, id, account_id, thread_id, received_at FROM representatives WHERE any_unread = 0
            ), ranked AS (
                SELECT section_id, id, account_id, thread_id, received_at,
                    ROW_NUMBER() OVER (PARTITION BY section_id ORDER BY received_at DESC, id DESC) AS section_rank
                FROM sectioned
            )
            SELECT section_id, id, account_id, thread_id, received_at
            FROM ranked WHERE section_rank <= ?
            ORDER BY section_id, section_rank"#
        );
        let mut statement = sqlx::query_as::<_, SmartConversationMatch>(&sql);
        for account_id in &query.account_ids {
            statement = statement.bind(account_id.to_string());
        }
        for account_id in &query.account_ids {
            statement = statement.bind(account_id.to_string());
        }
        let matches = statement
            .bind((limit + 1) as i64)
            .fetch_all(&self.pool)
            .await?;

        let mut by_section: HashMap<String, Vec<SmartConversationMatch>> = HashMap::new();
        for candidate in matches {
            by_section
                .entry(candidate.section_id.clone())
                .or_default()
                .push(candidate);
        }
        let mut selected = HashMap::new();
        let mut next_cursors = HashMap::new();
        let mut keys = Vec::new();
        for section_id in SECTION_IDS {
            let mut candidates = by_section.remove(section_id).unwrap_or_default();
            let has_more = candidates.len() > limit;
            candidates.truncate(limit);
            let next_cursor = has_more.then(|| {
                let last = candidates
                    .last()
                    .expect("a Smart section with more results has a cursor source");
                MailCursor {
                    received_at: last.received_at,
                    id: last.id.clone(),
                }
            });
            keys.extend(
                candidates
                    .iter()
                    .map(|candidate| (candidate.account_id.clone(), candidate.thread_id.clone())),
            );
            next_cursors.insert(section_id.to_owned(), next_cursor);
            selected.insert(section_id.to_owned(), candidates);
        }
        keys.sort();
        keys.dedup();
        let conversations = self
            .hydrate_conversations_by_keys(&keys, Some("INBOX"), false)
            .await?;

        Ok(SmartInboxPage {
            sections: SECTION_IDS
                .into_iter()
                .map(|section_id| SmartInboxSectionPage {
                    id: section_id.to_owned(),
                    conversations: selected
                        .remove(section_id)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|candidate| {
                            conversations
                                .get(&(candidate.account_id, candidate.thread_id))
                                .cloned()
                        })
                        .collect(),
                    next_cursor: next_cursors.remove(section_id).flatten(),
                })
                .collect(),
        })
    }

    async fn hydrate_conversations_by_keys(
        &self,
        keys: &[(String, String)],
        preferred_mailbox: Option<&str>,
        include_system_mailboxes: bool,
    ) -> Result<HashMap<(String, String), MailConversation>> {
        let mut hydrated = Vec::new();
        for chunk in keys.chunks(300) {
            let predicates = vec!["(account_id = ? AND thread_id = ?)"; chunk.len()].join(" OR ");
            let sql = format!("SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, '' AS body_text, NULL AS body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.is_answered, m.is_draft, m.has_attachments, m.category, m.classification_confidence, m.classification_source, '' AS classification_signals FROM messages m WHERE ({predicates}) ORDER BY m.received_at, m.id");
            let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
            for (account_id, thread_id) in chunk {
                statement = statement.bind(account_id).bind(thread_id);
            }
            hydrated.extend(statement.fetch_all(&self.pool).await?);
        }
        if !include_system_mailboxes {
            hydrated
                .retain(|message| !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"));
        }
        let mut grouped: HashMap<(String, String), Vec<MailSummary>> = HashMap::new();
        for message in hydrated {
            grouped
                .entry((message.account_id.clone(), message.thread_id.clone()))
                .or_default()
                .push(message);
        }
        Ok(grouped
            .into_iter()
            .filter_map(|((account_id, thread_id), source_messages)| {
                let messages =
                    deduplicate_message_copies(source_messages.clone(), preferred_mailbox);
                let latest = messages.last()?.clone();
                let mut participants = source_messages
                    .iter()
                    .map(|message| {
                        message
                            .from_name
                            .clone()
                            .unwrap_or_else(|| message.from_address.clone())
                    })
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                participants.sort();
                Some((
                    (account_id.clone(), thread_id.clone()),
                    MailConversation {
                        id: format!("{account_id}:{thread_id}"),
                        account_id,
                        thread_id,
                        message_count: messages.len(),
                        unread: source_messages.iter().any(|message| !message.is_read),
                        has_attachments: source_messages
                            .iter()
                            .any(|message| message.has_attachments),
                        participants,
                        latest,
                        messages,
                        source_messages,
                    },
                ))
            })
            .collect())
    }

    /// Searches candidate rows by a stable `(received_at, id)` keyset.  The
    /// caller supplies already-emitted conversations only for a session-bound
    /// continuation; direct callers retain the historical result-cursor path.
    /// This lets broad canonical predicates stop after a page plus lookahead
    /// instead of evaluating an entire mailbox on every keystroke.
    async fn search_conversation_matches_from_candidate(
        &self,
        query: &SearchQuery,
        candidate_cursor: Option<&MailCursor>,
        excluded_conversation_ids: &[String],
    ) -> Result<SearchConversationMatches> {
        // Existing list-state filters are conversation-wide, while the AST is
        // message-wide. Keep their complete-conversation path until it can be
        // compiled into an equally exact bounded predicate.
        if query.unread_only
            || query.read_only
            || query.flagged_only
            || query.unflagged_only
            || query.category.is_some()
        {
            return self.search_conversation_matches_full(query).await;
        }
        let limit = query.limit.unwrap_or(100).clamp(1, 501) as usize;
        let scan_chunk = SEARCH_CANDIDATE_SCAN_CHUNK;
        let expression = parse_search_query(&query.text)?;
        let mut expression_query = query.clone();
        expression_query.unread_only = false;
        expression_query.read_only = false;
        expression_query.flagged_only = false;
        expression_query.unflagged_only = false;
        expression_query.category = None;

        let mut cursor = candidate_cursor.cloned();
        let mut seen_threads = excluded_conversation_ids
            .iter()
            .filter_map(|id| id.split_once(':'))
            .map(|(account_id, thread_id)| (account_id.to_owned(), thread_id.to_owned()))
            .collect::<HashSet<_>>();
        let mut representatives = Vec::new();
        let mut last_scanned = cursor.clone();
        let mut scanned = 0usize;
        loop {
            let chunk = self
                .search_matching_message_chunk(
                    &expression_query,
                    &expression,
                    cursor.as_ref(),
                    None,
                    scan_chunk,
                )
                .await?;
            if chunk.rows.is_empty() {
                return Ok(SearchConversationMatches {
                    matched_by_thread: HashMap::new(),
                    representatives,
                    candidate_cursor: last_scanned,
                    exhausted: true,
                });
            }
            let chunk_last = chunk.rows.last().map(search_message_cursor);
            for (message, matched) in chunk.rows.iter().zip(&chunk.matches) {
                last_scanned = Some(search_message_cursor(message));
                scanned = scanned.saturating_add(1);
                if !matched {
                    continue;
                }
                let key = (message.account_id.clone(), message.thread_id.clone());
                if !seen_threads.insert(key) {
                    continue;
                }
                // A direct legacy cursor denotes the newest matching message
                // in a thread. When scanning from the top, remember threads
                // before that cursor as excluded so an older matching reply
                // cannot reintroduce a conversation on a later page.
                if candidate_cursor.is_none()
                    && query.cursor.as_ref().is_some_and(|result_cursor| {
                        message.received_at > result_cursor.received_at
                            || (message.received_at == result_cursor.received_at
                                && message.id >= result_cursor.id)
                    })
                {
                    continue;
                }
                representatives.push(message.clone());
                if representatives.len() >= limit {
                    let keys = representatives
                        .iter()
                        .map(|message| (message.account_id.clone(), message.thread_id.clone()))
                        .collect::<Vec<_>>();
                    let matched_by_thread = self
                        .search_matching_messages_for_threads(&expression_query, &expression, &keys)
                        .await?;
                    return Ok(SearchConversationMatches {
                        representatives,
                        matched_by_thread,
                        candidate_cursor: last_scanned,
                        exhausted: false,
                    });
                }
            }
            if scanned >= SEARCH_CANDIDATE_SCAN_BUDGET {
                let keys = representatives
                    .iter()
                    .map(|message| (message.account_id.clone(), message.thread_id.clone()))
                    .collect::<Vec<_>>();
                let matched_by_thread = self
                    .search_matching_messages_for_threads(&expression_query, &expression, &keys)
                    .await?;
                return Ok(SearchConversationMatches {
                    representatives,
                    matched_by_thread,
                    candidate_cursor: last_scanned,
                    exhausted: false,
                });
            }
            if chunk.exhausted {
                let keys = representatives
                    .iter()
                    .map(|message| (message.account_id.clone(), message.thread_id.clone()))
                    .collect::<Vec<_>>();
                let matched_by_thread = self
                    .search_matching_messages_for_threads(&expression_query, &expression, &keys)
                    .await?;
                return Ok(SearchConversationMatches {
                    representatives,
                    matched_by_thread,
                    candidate_cursor: last_scanned,
                    exhausted: true,
                });
            }
            cursor = chunk_last;
        }
    }

    async fn search_conversation_matches_full(
        &self,
        query: &SearchQuery,
    ) -> Result<SearchConversationMatches> {
        // Conversation pages request one look-ahead candidate, so this
        // internal query intentionally accepts 501 while the public page size
        // remains capped at 500.
        let limit = query.limit.unwrap_or(100).clamp(1, 501) as usize;
        let expression = parse_search_query(&query.text)?;
        // State and category filters are conversation properties in existing
        // list views. Evaluate the parsed expression first, then apply those
        // legacy scalar filters to the complete scoped conversation below.
        let mut expression_query = query.clone();
        expression_query.unread_only = false;
        expression_query.read_only = false;
        expression_query.flagged_only = false;
        expression_query.unflagged_only = false;
        expression_query.category = None;
        let matching = self
            .search_matching_messages(&expression_query, &expression)
            .await?;
        let mut representatives = Vec::new();
        let mut seen_threads = HashSet::new();
        let mut matched_by_thread: HashMap<(String, String), Vec<MailSummary>> = HashMap::new();
        for message in matching {
            let key = (message.account_id.clone(), message.thread_id.clone());
            if seen_threads.insert(key.clone()) {
                representatives.push(message.clone());
            }
            matched_by_thread.entry(key).or_default().push(message);
        }
        let keys = representatives
            .iter()
            .map(|message| (message.account_id.clone(), message.thread_id.clone()))
            .collect::<Vec<_>>();
        let isolated_special_mailbox = query
            .mailbox
            .as_deref()
            .is_some_and(is_special_mailbox_family);
        let include_system_mailboxes = query
            .mailbox
            .as_deref()
            .is_some_and(is_special_mailbox_family)
            || !positive_folder_scopes(&expression.root).is_empty();
        let conversations = if isolated_special_mailbox {
            HashMap::new()
        } else {
            self.hydrate_conversations_by_keys(
                &keys,
                query.mailbox.as_deref(),
                include_system_mailboxes,
            )
            .await?
        };
        representatives.retain(|representative| {
            if isolated_special_mailbox {
                return query
                    .category
                    .as_ref()
                    .is_none_or(|category| representative.category.as_ref() == Some(category))
                    && (!query.flagged_only || representative.is_flagged)
                    && (!query.unflagged_only || !representative.is_flagged)
                    && (!query.unread_only || !representative.is_read)
                    && (!query.read_only || representative.is_read);
            }
            let Some(conversation) = conversations.get(&(
                representative.account_id.clone(),
                representative.thread_id.clone(),
            )) else {
                return false;
            };
            let scoped = conversation
                .source_messages
                .iter()
                .filter(|message| {
                    query.mailbox.as_deref().map_or_else(
                        || {
                            include_system_mailboxes
                                || !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash")
                        },
                        |mailbox| {
                            if is_special_mailbox_family(mailbox) {
                                mailbox_family(&message.mailbox) == mailbox_family(mailbox)
                            } else {
                                message.mailbox == mailbox
                            }
                        },
                    )
                })
                .collect::<Vec<_>>();
            let Some(latest) = scoped.iter().max_by(|left, right| {
                left.received_at
                    .cmp(&right.received_at)
                    .then_with(|| left.id.cmp(&right.id))
            }) else {
                return false;
            };
            query
                .category
                .as_ref()
                .is_none_or(|category| latest.category.as_ref() == Some(category))
                && (!query.flagged_only || scoped.iter().any(|message| message.is_flagged))
                && (!query.unflagged_only || scoped.iter().all(|message| !message.is_flagged))
                && (!query.unread_only || scoped.iter().any(|message| !message.is_read))
                && (!query.read_only || scoped.iter().all(|message| message.is_read))
        });
        if let Some(cursor) = &query.cursor {
            representatives.retain(|message| {
                message.received_at < cursor.received_at
                    || (message.received_at == cursor.received_at && message.id < cursor.id)
            });
        }
        representatives.truncate(limit);
        Ok(SearchConversationMatches {
            representatives,
            matched_by_thread,
            candidate_cursor: None,
            exhausted: true,
        })
    }

    pub async fn messages_by_ids(&self, ids: &[String]) -> Result<Vec<MailSummary>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id IN ({placeholders}) ORDER BY received_at");
        let mut query = sqlx::query_as::<_, MailSummary>(&sql);
        for id in ids {
            query = query.bind(id);
        }
        Ok(query.fetch_all(&self.pool).await?)
    }

    pub async fn message_by_locator(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
    ) -> Result<Option<MailSummary>> {
        Ok(sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn set_message_category(&self, id: &str, category: &str) -> Result<()> {
        if !matches!(
            category,
            "people" | "transactions" | "notifications" | "newsletters" | "other"
        ) {
            return Err(anyhow!("unknown mail category"));
        }
        sqlx::query("UPDATE messages SET category = ?, classification_confidence = 1, classification_source = 'user' WHERE id = ?")
            .bind(category)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Newly synced messages that have not yet been classified.
    pub async fn messages_for_model_classification(&self) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE classification_source IS NULL AND content_state = 'complete' ORDER BY received_at DESC";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .fetch_all(&self.pool)
            .await?)
    }

    /// One bounded FIFO batch of newly synced complete messages. Applying the
    /// model result removes rows from this selection, so callers can safely
    /// repeat it until it is empty without retaining the whole inbox in RAM.
    pub async fn messages_for_model_classification_batch(
        &self,
        limit: usize,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE classification_source IS NULL AND content_state = 'complete' ORDER BY received_at DESC, id DESC LIMIT ?";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(limit.clamp(1, 1_000) as i64)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Returns candidate message IDs whose sender is an RFC-parsed recipient
    /// of account-local Sent mail. This is stronger People evidence than a
    /// sender-controlled Reply-To, In-Reply-To, or reply-shaped subject.
    pub async fn messages_from_known_correspondents(
        &self,
        messages: &[MailSummary],
    ) -> Result<HashSet<String>> {
        if messages.is_empty() {
            return Ok(HashSet::new());
        }
        let placeholders = vec!["?"; messages.len()].join(",");
        let sql = format!(
            "SELECT candidate.id FROM messages candidate JOIN sent_correspondents known ON known.account_id = candidate.account_id AND known.address = candidate.from_address WHERE candidate.id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for message in messages {
            query = query.bind(&message.id);
        }
        Ok(query.fetch_all(&self.pool).await?.into_iter().collect())
    }

    /// Messages eligible for an explicitly requested model reclassification.
    /// User-selected categories are deliberately excluded.
    pub async fn messages_for_model_reclassification(&self) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE content_state = 'complete' AND (classification_source IS NULL OR classification_source = 'model') ORDER BY received_at DESC";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn mailbox_signal_metadata(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Vec<MailSignalMetadata>> {
        Ok(sqlx::query_as::<_, MailSignalMetadata>(
            "SELECT id, uid, classification_signals FROM messages WHERE account_id = ? AND mailbox = ? ORDER BY uid",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn update_classification_signals(&self, updates: &[(String, String)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (id, signals) in updates {
            // Evidence changes invalidate only model-owned decisions. Keep the
            // visible category until the background classifier replaces it,
            // and never override a user's explicit categorization.
            sqlx::query("UPDATE messages SET classification_source = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_source END, classification_confidence = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_confidence END, classification_signals = ? WHERE id = ?")
                .bind(signals)
                .bind(signals)
                .bind(signals)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn apply_model_classifications(
        &self,
        classifications: &[ModelClassificationUpdate],
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        let mut applied = 0;
        // Acquire the SQLite write reservation before checking evidence so a
        // concurrent sync cannot change signals or Sent relationships between
        // validation and commit.
        sqlx::query(
            "UPDATE app_meta SET value = value WHERE key = 'classification_policy_version'",
        )
        .execute(&mut *tx)
        .await?;
        for update in classifications {
            let result = sqlx::query("UPDATE messages SET category = ?, classification_confidence = ?, classification_source = 'model' WHERE id = ? AND from_name IS ? AND from_address = ? AND subject = ? AND snippet = ? AND body_text = ? AND classification_signals = ? AND EXISTS(SELECT 1 FROM sent_correspondents known WHERE known.account_id = messages.account_id AND known.address = messages.from_address) = ? AND EXISTS(SELECT 1 FROM app_meta WHERE key = 'classification_revision_owner' AND value = ?) AND EXISTS(SELECT 1 FROM app_meta WHERE key = 'classification_model_revision' AND value = ?) AND EXISTS(SELECT 1 FROM app_meta WHERE key = 'classification_policy_version' AND value = ?) AND (classification_source IS NULL OR classification_source = 'model')")
                .bind(&update.category)
                .bind(update.confidence)
                .bind(&update.id)
                .bind(&update.expected_from_name)
                .bind(&update.expected_from_address)
                .bind(&update.expected_subject)
                .bind(&update.expected_snippet)
                .bind(&update.expected_body_text)
                .bind(&update.expected_signals)
                .bind(update.expected_known_correspondence)
                .bind(&update.expected_owner)
                .bind(&update.expected_model_revision)
                .bind(update.expected_policy_revision)
                .execute(&mut *tx)
                .await?;
            applied += result.rows_affected() as usize;
        }
        tx.commit().await?;
        Ok(applied)
    }

    pub async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>> {
        sqlx::query_as::<_, Attachment>("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe FROM attachments WHERE message_id = ? AND presentation IN ('downloadable', 'both') ORDER BY filename COLLATE NOCASE, id")
            .bind(message_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn attachment_data(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<AttachmentData> {
        let row: (String, String, String, String, i64, bool, AttachmentPresentation, bool, Vec<u8>) = sqlx::query_as("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe, data FROM attachments WHERE message_id = ? AND id = ? AND presentation IN ('downloadable', 'both')")
            .bind(message_id)
            .bind(attachment_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(AttachmentData {
            attachment: Attachment {
                id: row.0,
                message_id: row.1,
                filename: row.2,
                mime_type: row.3,
                size_bytes: row.4,
                is_inline: row.5,
                presentation: row.6,
                is_potentially_unsafe: row.7,
            },
            bytes: row.8,
        })
    }

    async fn rebuild_threads_for_messages(&self, messages: &[MailSummary]) -> Result<()> {
        let accounts = messages
            .iter()
            .map(|message| message.account_id.as_str())
            .collect::<HashSet<_>>();
        for account_id in accounts {
            self.rebuild_threads_for_account(account_id).await?;
        }
        Ok(())
    }

    async fn rebuild_threads_for_account(&self, account_id: &str) -> Result<()> {
        let rows = sqlx::query_as::<_, ThreadRow>("SELECT id, thread_id, message_id, in_reply_to, reference_ids, subject, from_address, to_addresses, received_at FROM messages WHERE account_id = ? ORDER BY received_at, id")
            .bind(account_id)
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            return Ok(());
        }

        let mut groups = DisjointSet::new(rows.len());
        let mut by_message_id: HashMap<String, usize> = HashMap::new();
        for (index, row) in rows.iter().enumerate() {
            if let Some(message_id) = row.message_id.as_deref().and_then(normalize_message_id) {
                if let Some(existing) = by_message_id.insert(message_id, index) {
                    groups.union(index, existing);
                }
            }
        }
        let mut by_linked_id: HashMap<String, usize> = HashMap::new();
        for (index, row) in rows.iter().enumerate() {
            let linked_ids = row
                .in_reply_to
                .iter()
                .chain(row.reference_ids.iter())
                .flat_map(|value| parse_message_ids(value))
                .collect::<Vec<_>>();
            for linked_id in linked_ids {
                if let Some(parent) = by_message_id.get(&linked_id) {
                    groups.union(index, *parent);
                }
                if let Some(sibling) = by_linked_id.insert(linked_id, index) {
                    groups.union(index, sibling);
                }
            }
        }

        // Some clients omit References. Only use a normalized-subject fallback
        // when the subject is explicitly reply/forward-shaped and participants
        // overlap, avoiding accidental grouping of recurring newsletters.
        for index in 0..rows.len() {
            if !is_reply_subject(&rows[index].subject)
                || rows[index].in_reply_to.is_some()
                || rows[index].reference_ids.is_some()
            {
                continue;
            }
            let subject = normalized_subject(&rows[index].subject);
            if subject.is_empty() {
                continue;
            }
            if let Some(parent) = (0..index).rev().find(|candidate| {
                normalized_subject(&rows[*candidate].subject) == subject
                    && participants_overlap(&rows[index], &rows[*candidate])
                    && (rows[index].received_at - rows[*candidate].received_at).num_days() <= 30
            }) {
                groups.union(index, parent);
            }
        }

        let mut roots: HashMap<usize, String> = HashMap::new();
        for (index, row) in rows.iter().enumerate() {
            let root = groups.find(index);
            roots.entry(root).or_insert_with(|| {
                row.reference_ids
                    .as_deref()
                    .and_then(normalize_message_id)
                    .or_else(|| row.in_reply_to.as_deref().and_then(normalize_message_id))
                    .or_else(|| row.message_id.as_deref().and_then(normalize_message_id))
                    .unwrap_or_else(|| row.id.clone())
            });
        }
        let mut updates = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let thread_id = &roots[&groups.find(index)];
            if row.thread_id.as_deref() != Some(thread_id.as_str()) {
                updates.push((row.id.as_str(), thread_id.as_str()));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (id, thread_id) in updates {
            sqlx::query("UPDATE messages SET thread_id = ? WHERE id = ?")
                .bind(thread_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn target_allows_mailbox(message: &MailSummary, target: &ConversationTarget) -> bool {
    match target.mailbox.as_deref() {
        Some(mailbox) if matches!(mailbox_family(mailbox), "Spam" | "Trash") => {
            mailbox_family(&message.mailbox) == mailbox_family(mailbox)
        }
        Some(_) | None => !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"),
    }
}

fn mail_conversation_from_sources(
    account_id: String,
    thread_id: String,
    source_messages: Vec<MailSummary>,
    preferred_mailbox: Option<&str>,
) -> Option<MailConversation> {
    // Keep every durable locator for mutation fan-out. `messages` below is
    // only the mailbox-preferred reader/list projection, so it may
    // intentionally collapse logical Message-ID copies.
    let messages = deduplicate_message_copies(source_messages.clone(), preferred_mailbox);
    let latest = messages.last()?.clone();
    let mut participants = source_messages
        .iter()
        .map(|message| {
            message
                .from_name
                .clone()
                .unwrap_or_else(|| message.from_address.clone())
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    participants.sort();
    Some(MailConversation {
        id: format!("{account_id}:{thread_id}"),
        account_id,
        thread_id,
        message_count: messages.len(),
        unread: source_messages.iter().any(|message| !message.is_read),
        has_attachments: source_messages
            .iter()
            .any(|message| message.has_attachments),
        participants,
        latest,
        messages,
        source_messages,
    })
}

fn deduplicate_message_copies(
    mut messages: Vec<MailSummary>,
    preferred_mailbox: Option<&str>,
) -> Vec<MailSummary> {
    messages.sort_by_key(|message| {
        (
            message.received_at,
            preferred_mailbox.is_none_or(|mailbox| mailbox_family(&message.mailbox) != mailbox),
            message.id.clone(),
        )
    });
    let mut seen = HashSet::new();
    messages.retain(|message| {
        let key = message
            .message_id
            .as_deref()
            .and_then(normalize_message_id)
            .unwrap_or_else(|| message.id.clone());
        seen.insert(key)
    });
    messages.sort_by(|left, right| {
        left.received_at
            .cmp(&right.received_at)
            .then(left.id.cmp(&right.id))
    });
    messages
}

fn mailbox_family(mailbox: &str) -> &str {
    mailbox
        .split_once("::")
        .map_or(mailbox, |(family, _)| family)
}

fn is_special_mailbox_family(mailbox: &str) -> bool {
    matches!(mailbox, "Sent" | "Drafts" | "Archive" | "Spam" | "Trash")
}

#[derive(FromRow)]
struct ContactedPeopleBackfillRow {
    message_id: String,
    rfc_message_id: Option<String>,
    mailbox: String,
    uid: i64,
    mailbox_uid_validity: Option<i64>,
    to_addresses: String,
    cc_addresses: String,
    bcc_addresses: String,
    is_draft: bool,
    received_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct ContactedPeopleSentProviderCutoff {
    mailbox: String,
    uid_validity: i64,
    cutoff_uid: i64,
}

async fn account_search_generation_matches_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    generation: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts a JOIN account_search_generations g ON g.account_id = a.id WHERE a.id = ? AND g.generation = ?)",
    )
    .bind(account_id)
    .bind(generation)
    .fetch_one(&mut **tx)
    .await?)
}

/// Advances a live account's provider-search generation inside the caller's
/// already authoritative transaction. Legacy catalogue rows can outlive their
/// account record in older profiles; normal local read/star maintenance must
/// still be able to repair those rows. A deletion tombstone is different: it
/// is an explicit fence and must continue to reject every late local write.
async fn advance_account_search_generation_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
) -> Result<()> {
    let removed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await?;
    if removed {
        return Err(anyhow!("account was removed"));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
        .bind(account_id)
        .fetch_one(&mut **tx)
        .await?;
    if !exists {
        // This is a pre-generation legacy/orphan row, not an account that
        // exists for provider search. Do not create a generation row: guarded
        // provider publication remains unable to revive it.
        return Ok(());
    }
    sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) SELECT ?, 0 WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?)")
        .bind(account_id)
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    let updated = sqlx::query(
        "UPDATE account_search_generations SET generation = generation + 1 WHERE account_id = ?",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    debug_assert_eq!(updated, 1, "existing account must own a generation row");
    Ok(())
}

async fn ensure_live_contacted_people_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
) -> Result<()> {
    let removed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await?;
    if removed {
        return Err(anyhow!("account was removed"));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
        .bind(account_id)
        .fetch_one(&mut **tx)
        .await?;
    if !exists {
        return Err(anyhow!("account does not exist"));
    }
    Ok(())
}

/// Legacy marker rows persisted only a stable message ID. For an evicted
/// Message-ID-less Sent row, recover its exact old locator only when the ID
/// format is reversible; the selectable catalogue below still provides the
/// authority needed to map it to an opaque mailbox.
fn legacy_contacted_people_marker_locator(
    account_id: &str,
    message_id: &str,
) -> Option<(String, i64, bool)> {
    let prefix = format!("{account_id}:");
    let encoded = message_id.strip_prefix(&prefix)?;
    let (mailbox, uid, is_v2) = if let Some(encoded) = encoded.strip_prefix("v2:") {
        let (mailbox, uid) = encoded.rsplit_once(':')?;
        let mailbox = String::from_utf8(URL_SAFE_NO_PAD.decode(mailbox).ok()?).ok()?;
        (mailbox, uid, true)
    } else {
        let (mailbox, uid) = encoded.rsplit_once(':')?;
        // Before v2, `stable_message_id` replaced colons with underscores.
        // Keep that encoded segment intact until selectable metadata proves
        // the exact original locator, since underscores themselves are legal.
        (mailbox.to_owned(), uid, false)
    };
    Some((mailbox, uid.parse().ok()?, is_v2))
}

async fn resolve_evicted_legacy_contacted_people_source_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    marker_mailbox: &str,
    marker_is_v2: bool,
) -> Result<Option<(String, Option<i64>)>> {
    let candidates: Vec<LegacyContactedPeopleMailboxCandidate> = sqlx::query_as(
        "SELECT remote_path, local_path, special_use, hierarchy_delimiter, uid_validity \
         FROM selectable_mailboxes WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut matches = Vec::new();
    for (remote_path, local_path, special_use, delimiter, uid_validity) in candidates {
        let special = special_use.as_deref().map(str::to_ascii_lowercase);
        let special_family = match special.as_deref() {
            Some("\\sent") => Some("Sent"),
            Some("\\drafts") => Some("Drafts"),
            Some("\\archive") | Some("\\all") => Some("Archive"),
            Some("\\junk") | Some("\\spam") => Some("Spam"),
            Some("\\trash") => Some("Trash"),
            _ => None,
        };
        let (legacy_mailbox, target) = match special_family {
            Some(family) => (
                format!("{family}::{remote_path}"),
                special_mailbox_storage_identity(family, &remote_path),
            ),
            None => (
                remote_path.clone(),
                generic_mailbox_storage_identity(
                    &remote_path,
                    &opaque_mailbox_display_path(&remote_path, delimiter.as_deref()),
                ),
            ),
        };
        let marker_matches = if marker_is_v2 {
            marker_mailbox == legacy_mailbox
        } else {
            marker_mailbox == legacy_mailbox.replace(':', "_")
        };
        // The opaque migration may have completed its first locator batch
        // before this legacy marker batch runs. Accept either the original
        // authoritative local alias or its computed opaque successor.
        if marker_matches && (local_path == legacy_mailbox || local_path == target) {
            matches.push((target, uid_validity));
        }
    }
    matches.sort();
    matches.dedup();
    Ok((matches.len() == 1).then(|| matches.remove(0)))
}

fn is_contacted_people_sent_mailbox(mailbox: &str) -> bool {
    matches!(mailbox_family(mailbox), "Sent")
}

async fn contacted_people_collection_generation_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<i64> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_meta WHERE key = 'contacted_people_collection_generation'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    value
        .map(|value| {
            value
                .parse::<i64>()
                .context("contacted-people collection generation is invalid")
        })
        .transpose()
        .map(|value| value.unwrap_or(0))
}

async fn next_contacted_people_action_sequence_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
) -> Result<i64> {
    let current: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_meta WHERE key = 'contacted_people_action_sequence'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    let next = current
        .as_deref()
        .unwrap_or("0")
        .parse::<i64>()
        .context("contacted-people action sequence is invalid")?
        .checked_add(1)
        .ok_or_else(|| anyhow!("contacted-people action sequence overflow"))?;
    sqlx::query("INSERT INTO app_meta(key, value) VALUES ('contacted_people_action_sequence', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(next.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(next)
}

async fn advance_contacted_people_collection_generation_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<i64> {
    let generation = contacted_people_collection_generation_in_tx(tx).await?;
    let next = generation
        .checked_add(1)
        .ok_or_else(|| anyhow!("contacted-people collection generation overflow"))?;
    sqlx::query("INSERT INTO app_meta(key, value) VALUES ('contacted_people_collection_generation', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(next.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(next)
}

async fn contacted_people_sent_provider_cutoffs_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    generation: i64,
) -> Result<Vec<ContactedPeopleSentProviderCutoff>> {
    Ok(sqlx::query_as("SELECT mailbox, uid_validity, cutoff_uid FROM contacted_people_sent_provider_cutoffs WHERE account_id = ? AND generation = ?")
        .bind(account_id)
        .bind(generation)
        .fetch_all(&mut **tx)
        .await?)
}

async fn autocomplete_suggestions_enabled_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<bool> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_meta WHERE key = 'autocomplete_suggestions_enabled'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(!matches!(value.as_deref(), Some("0") | Some("false")))
}

async fn contacted_people_clear_sequence_in_tx(tx: &mut Transaction<'_, Sqlite>) -> Result<i64> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_meta WHERE key = 'contacted_people_clear_sequence'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    value
        .as_deref()
        .unwrap_or("0")
        .parse()
        .context("contacted-people clear sequence is invalid")
}

async fn remove_contacted_people_account_contribution_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
) -> Result<()> {
    for statement in [
        "DELETE FROM contacted_people_backfill_messages WHERE account_id = ?",
        "DELETE FROM contacted_people_legacy_unresolved_sources WHERE account_id = ?",
        "DELETE FROM contacted_people_backfill_sources WHERE account_id = ?",
        "DELETE FROM contacted_people_backfill_rfc_messages WHERE account_id = ?",
        "DELETE FROM contacted_people_backfill_progress WHERE account_id = ?",
        "DELETE FROM contacted_people_outgoing_messages WHERE account_id = ?",
        "DELETE FROM contacted_people_sent_provider_cutoffs WHERE account_id = ?",
        "DELETE FROM contacted_people_account_stats WHERE account_id = ?",
    ] {
        sqlx::query(statement)
            .bind(account_id)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "UPDATE contacted_people SET first_contacted_at = (SELECT MIN(stats.first_contacted_at) FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1), last_contacted_at = (SELECT MAX(stats.last_contacted_at) FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1), send_count = (SELECT SUM(stats.send_count) FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1), display_name = (SELECT stats.display_name FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1 ORDER BY stats.last_contacted_at DESC, stats.account_id ASC LIMIT 1), formatted_address = COALESCE((SELECT stats.formatted_address FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1 ORDER BY stats.last_contacted_at DESC, stats.account_id ASC LIMIT 1), contacted_people.canonical_address) WHERE EXISTS (SELECT 1 FROM contacted_people_account_stats stats JOIN accounts account ON account.id = stats.account_id WHERE stats.canonical_address = contacted_people.canonical_address AND json_extract(account.data, '$.enabled') = 1)",
    )
    .execute(&mut **tx)
    .await?;
    let remaining_people: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT canonical_address, display_name FROM contacted_people")
            .fetch_all(&mut **tx)
            .await?;
    for (address, display_name) in remaining_people {
        update_contacted_people_normalized_search_in_tx(tx, &address, display_name.as_deref())
            .await?;
    }
    sqlx::query(
        "DELETE FROM contacted_people WHERE NOT EXISTS (SELECT 1 FROM contacted_people_account_stats stats WHERE stats.canonical_address = contacted_people.canonical_address)",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn recompute_enabled_contacted_people_aggregate_for_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    canonical_address: &str,
) -> Result<()> {
    sqlx::query("UPDATE contacted_people SET first_contacted_at = (SELECT MIN(s.first_contacted_at) FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1), last_contacted_at = (SELECT MAX(s.last_contacted_at) FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1), send_count = (SELECT SUM(s.send_count) FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1), display_name = (SELECT s.display_name FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1 ORDER BY s.last_contacted_at DESC, s.account_id ASC LIMIT 1), formatted_address = COALESCE((SELECT s.formatted_address FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1 ORDER BY s.last_contacted_at DESC, s.account_id ASC LIMIT 1), formatted_address) WHERE canonical_address = ? AND EXISTS (SELECT 1 FROM contacted_people_account_stats s JOIN accounts a ON a.id = s.account_id WHERE s.canonical_address = contacted_people.canonical_address AND json_extract(a.data, '$.enabled') = 1)")
    .bind(canonical_address)
    .execute(&mut **tx)
    .await?;
    let display_name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM contacted_people WHERE canonical_address = ?")
            .bind(canonical_address)
            .fetch_optional(&mut **tx)
            .await?;
    update_contacted_people_normalized_search_in_tx(tx, canonical_address, display_name.as_deref())
        .await?;
    Ok(())
}

async fn contacted_people_migration_complete_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    complete_key: &str,
) -> Result<bool> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
        .bind(complete_key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(matches!(value.as_deref(), Some("1")))
}

async fn contacted_people_migration_cursor_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    cursor_key: &str,
) -> Result<i64> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
        .bind(cursor_key)
        .fetch_optional(&mut **tx)
        .await?;
    // A damaged progress value must not make an upgrade skip unknown rows.
    // Reprocessing is idempotent and the subsequent bounded batch repairs it.
    Ok(value
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(0))
}

async fn finish_contacted_people_migration_batch_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    cursor_key: &str,
    complete_key: &str,
    last_rowid: Option<i64>,
    has_more: bool,
) -> Result<()> {
    if has_more {
        let last_rowid = last_rowid
            .ok_or_else(|| anyhow!("migration reported remaining rows without a cursor"))?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(cursor_key)
            .bind(last_rowid.to_string())
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("DELETE FROM app_meta WHERE key = ?")
            .bind(cursor_key)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO app_meta(key, value) VALUES (?, '1') ON CONFLICT(key) DO UPDATE SET value = '1'")
            .bind(complete_key)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn record_contacted_people_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    recipients: &[ContactedPersonRecipient],
    excluded_addresses: &[String],
    contacted_at: DateTime<Utc>,
    restore_hidden: bool,
    accepted_sequence: Option<i64>,
) -> Result<usize> {
    let mut excluded: HashSet<String> = excluded_addresses
        .iter()
        .filter_map(|address| canonical_contacted_address(address))
        .collect();
    // Every configured account is an owner address, not a contacted person.
    // Query this in the transaction instead of trusting every call site to
    // supply a complete account list, including accounts added later.
    let account_addresses: Vec<String> = sqlx::query_scalar("SELECT email FROM accounts")
        .fetch_all(&mut **tx)
        .await?;
    for address in account_addresses {
        if let Some(address) = canonical_contacted_address(&address) {
            excluded.insert(address);
        }
    }

    let mut unique = HashMap::<String, ContactedPersonRecipient>::new();
    for recipient in recipients {
        let Some(address) = canonical_contacted_address(&recipient.address) else {
            continue;
        };
        if !excluded.contains(&address) {
            unique.insert(address, recipient.clone());
        }
    }

    for (address, recipient) in &unique {
        let display_name = sanitized_display_name(recipient.display_name.as_deref());
        let formatted_address = sanitized_formatted_address(
            recipient.formatted_address.as_deref(),
            display_name.as_deref(),
            address,
        );
        sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES (?, ?, ?, ?, ?, 1, NULL) ON CONFLICT(canonical_address) DO UPDATE SET display_name = CASE WHEN excluded.last_contacted_at >= contacted_people.last_contacted_at THEN excluded.display_name ELSE contacted_people.display_name END, formatted_address = CASE WHEN excluded.last_contacted_at >= contacted_people.last_contacted_at THEN excluded.formatted_address ELSE contacted_people.formatted_address END, first_contacted_at = CASE WHEN excluded.first_contacted_at < contacted_people.first_contacted_at THEN excluded.first_contacted_at ELSE contacted_people.first_contacted_at END, last_contacted_at = CASE WHEN excluded.last_contacted_at > contacted_people.last_contacted_at THEN excluded.last_contacted_at ELSE contacted_people.last_contacted_at END, send_count = contacted_people.send_count + 1, hidden_at = CASE WHEN ? AND (? IS NULL OR contacted_people.hidden_sequence < ?) AND (contacted_people.hidden_at IS NULL OR contacted_people.hidden_at < excluded.last_contacted_at) THEN NULL ELSE contacted_people.hidden_at END")
            .bind(address)
            .bind(&display_name)
            .bind(&formatted_address)
            .bind(contacted_at)
            .bind(contacted_at)
            .bind(restore_hidden)
            .bind(accepted_sequence)
            .bind(accepted_sequence)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count, display_name, formatted_address) VALUES (?, ?, ?, ?, 1, ?, ?) ON CONFLICT(canonical_address, account_id) DO UPDATE SET first_contacted_at = CASE WHEN excluded.first_contacted_at < contacted_people_account_stats.first_contacted_at THEN excluded.first_contacted_at ELSE contacted_people_account_stats.first_contacted_at END, last_contacted_at = CASE WHEN excluded.last_contacted_at > contacted_people_account_stats.last_contacted_at THEN excluded.last_contacted_at ELSE contacted_people_account_stats.last_contacted_at END, send_count = contacted_people_account_stats.send_count + 1, display_name = CASE WHEN excluded.last_contacted_at >= contacted_people_account_stats.last_contacted_at THEN excluded.display_name ELSE contacted_people_account_stats.display_name END, formatted_address = CASE WHEN excluded.last_contacted_at >= contacted_people_account_stats.last_contacted_at THEN excluded.formatted_address ELSE contacted_people_account_stats.formatted_address END")
            .bind(address)
            .bind(account_id)
            .bind(contacted_at)
            .bind(contacted_at)
            .bind(display_name)
            .bind(formatted_address)
            .execute(&mut **tx)
            .await?;
        recompute_enabled_contacted_people_aggregate_for_in_tx(tx, address).await?;
        let stored_display_name: Option<String> = sqlx::query_scalar(
            "SELECT display_name FROM contacted_people WHERE canonical_address = ?",
        )
        .bind(address)
        .fetch_one(&mut **tx)
        .await?;
        update_contacted_people_normalized_search_in_tx(
            tx,
            address,
            stored_display_name.as_deref(),
        )
        .await?;
    }
    Ok(unique.len())
}

fn canonical_contacted_address(address: &str) -> Option<String> {
    let address = address.trim();
    if address.is_empty()
        || address
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return None;
    }
    let (local, domain) = address.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(address.to_lowercase())
}

fn sanitized_display_name(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
        .map(ToOwned::to_owned)
}

fn sanitized_formatted_address(
    formatted_address: Option<&str>,
    display_name: Option<&str>,
    address: &str,
) -> String {
    formatted_address
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format_contacted_address(display_name, address))
}

fn format_contacted_address(display_name: Option<&str>, address: &str) -> String {
    let Some(display_name) = display_name else {
        return address.to_owned();
    };
    if display_name
        .chars()
        .any(|character| matches!(character, ',' | ';' | '<' | '>' | '"' | '\\'))
    {
        return format!(
            "\"{}\" <{address}>",
            display_name.replace('\\', "\\\\").replace('"', "\\\"")
        );
    }
    format!("{display_name} <{address}>")
}

fn parse_contacted_people_headers(headers: &[&str]) -> Vec<ContactedPersonRecipient> {
    let mut recipients = Vec::new();
    for value in headers {
        let value = value.trim();
        if value.is_empty() || value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
            continue;
        }
        let raw = format!("To: {value}\r\n\r\n");
        let Some(parsed) = MessageParser::default().parse_headers(raw.as_bytes()) else {
            continue;
        };
        let Some(addresses) = parsed
            .header(HeaderName::To)
            .and_then(HeaderValue::as_address)
        else {
            continue;
        };
        match addresses {
            ParsedAddress::List(addresses) => {
                recipients.extend(addresses.iter().filter_map(|address| {
                    address
                        .address
                        .as_ref()
                        .map(|email| ContactedPersonRecipient {
                            address: email.to_string(),
                            display_name: address.name.as_ref().map(ToString::to_string),
                            formatted_address: None,
                        })
                }))
            }
            ParsedAddress::Group(groups) => recipients.extend(
                groups
                    .iter()
                    .flat_map(|group| group.addresses.iter())
                    .filter_map(|address| {
                        address
                            .address
                            .as_ref()
                            .map(|email| ContactedPersonRecipient {
                                address: email.to_string(),
                                display_name: address.name.as_ref().map(ToString::to_string),
                                formatted_address: None,
                            })
                    }),
            ),
        }
    }
    recipients
}

fn normalize_contacted_people_match(value: &str) -> String {
    value
        .trim()
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_contacted_people_tokens(value: &str) -> String {
    let normalized = normalize_contacted_people_match(value);
    let words = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty() {
        String::new()
    } else {
        format!(" {} ", words.join(" "))
    }
}

async fn update_contacted_people_normalized_search_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    address: &str,
    display_name: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE contacted_people SET normalized_display_name = ?, normalized_address = ?, normalized_display_tokens = ?, normalized_address_tokens = ? WHERE canonical_address = ?")
        .bind(display_name.map(normalize_contacted_people_match).unwrap_or_default())
        .bind(normalize_contacted_people_match(address))
        .bind(display_name.map(normalize_contacted_people_tokens).unwrap_or_default())
        .bind(normalize_contacted_people_tokens(address))
        .bind(address)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(FromRow)]
struct ThreadRow {
    id: String,
    thread_id: Option<String>,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    reference_ids: Option<String>,
    subject: String,
    from_address: String,
    to_addresses: String,
    received_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct SmartConversationMatch {
    section_id: String,
    id: String,
    account_id: String,
    thread_id: String,
    received_at: DateTime<Utc>,
}

struct DisjointSet(Vec<usize>);

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self((0..len).collect())
    }
    fn find(&mut self, index: usize) -> usize {
        if self.0[index] != index {
            self.0[index] = self.find(self.0[index]);
        }
        self.0[index]
    }
    fn union(&mut self, left: usize, right: usize) {
        let left = self.find(left);
        let right = self.find(right);
        if left != right {
            self.0[right] = left;
        }
    }
}

fn normalize_message_id(value: &str) -> Option<String> {
    parse_message_ids(value).into_iter().next()
}

fn parse_message_ids(value: &str) -> Vec<String> {
    let mut remaining = value.trim_start();
    let mut ids = Vec::new();
    while !remaining.is_empty() {
        let Some(after_open) = remaining.strip_prefix('<') else {
            return Vec::new();
        };
        let Some(end) = after_open.find('>') else {
            return Vec::new();
        };
        ids.push(after_open[..end].to_ascii_lowercase());
        remaining = after_open[end + 1..].trim_start();
    }
    ids
}

fn normalized_subject(subject: &str) -> String {
    let mut value = subject.trim();
    while let Some((prefix, rest)) = value.split_once(':') {
        if matches!(
            prefix.trim().to_ascii_lowercase().as_str(),
            "re" | "fw" | "fwd"
        ) {
            value = rest.trim();
        } else {
            break;
        }
    }
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn is_reply_subject(subject: &str) -> bool {
    normalized_subject(subject)
        != subject
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
}

fn participants_overlap(left: &ThreadRow, right: &ThreadRow) -> bool {
    let left = format!("{} {}", left.from_address, left.to_addresses).to_ascii_lowercase();
    let right_from = right.from_address.to_ascii_lowercase();
    let left_from = left.split_whitespace().next().unwrap_or_default();
    left.contains(&right_from) || right.to_addresses.to_ascii_lowercase().contains(left_from)
}

#[derive(Clone, Copy)]
enum FlagUpdatePolicy {
    ProviderAuthoritative,
    CompareAndSwap(Option<(bool, bool)>),
}

async fn persist_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    message: &MailSummary,
) -> Result<()> {
    persist_message_with_flag_policy(tx, message, FlagUpdatePolicy::ProviderAuthoritative).await
}

async fn replace_uidvalidity_catalog_messages_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    messages: &[MailSummary],
) -> Result<()> {
    let mut scopes = HashSet::new();
    for message in messages {
        scopes.insert((message.account_id.as_str(), message.mailbox.as_str()));
    }
    for (account_id, mailbox) in scopes {
        // A successful UIDVALIDITY replacement starts a new namespace;
        // tombstones from the old one must not suppress replacement rows.
        sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?")
            .bind(account_id)
            .bind(mailbox)
            .execute(&mut **tx)
            .await?;
    }
    for message in messages {
        for table in [
            "message_content_fetches",
            "message_content_cache",
            "starred_attachment_metadata",
            "starred_message_bodies",
            "attachments",
        ] {
            let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)");
            sqlx::query(&statement)
                .bind(&message.account_id)
                .bind(&message.mailbox)
                .bind(message.uid)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(&message.account_id)
            .bind(&message.mailbox)
            .bind(message.uid)
            .execute(&mut **tx)
            .await?;
        persist_message(tx, message).await?;
    }
    Ok(())
}

async fn clear_uidvalidity_replacement_namespace_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    mailbox: &str,
) -> Result<()> {
    for table in [
        "message_content_fetches",
        "message_content_cache",
        "starred_attachment_metadata",
        "starred_message_bodies",
        "attachments",
    ] {
        let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ?)");
        sqlx::query(&statement)
            .bind(account_id)
            .bind(mailbox)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ?")
        .bind(account_id)
        .bind(mailbox)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?")
        .bind(account_id)
        .bind(mailbox)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Streams one replacement namespace from generation staging. Each query is
/// keyset-bounded so finalization does not turn a large mailbox rebuild into a
/// second in-memory catalogue.
async fn persist_staged_uidvalidity_replacement_messages_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    mailbox: &str,
    generation: &str,
) -> Result<()> {
    let mut after_uid: Option<i64> = None;
    loop {
        let rows: Vec<(i64, String)> = match after_uid {
            Some(after_uid) => sqlx::query_as(
                "SELECT message.uid, message.message_json FROM mailbox_snapshot_messages AS message JOIN mailbox_snapshot_items AS item ON item.account_id = message.account_id AND item.mailbox = message.mailbox AND item.generation = message.generation AND item.uid = message.uid WHERE message.account_id = ? AND message.mailbox = ? AND message.generation = ? AND message.uid > ? ORDER BY message.uid ASC LIMIT ?",
            )
            .bind(account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(after_uid)
            .bind(MAILBOX_SNAPSHOT_REPLACEMENT_PUBLISH_BATCH_SIZE)
            .fetch_all(&mut **tx)
            .await?,
            None => sqlx::query_as(
                "SELECT message.uid, message.message_json FROM mailbox_snapshot_messages AS message JOIN mailbox_snapshot_items AS item ON item.account_id = message.account_id AND item.mailbox = message.mailbox AND item.generation = message.generation AND item.uid = message.uid WHERE message.account_id = ? AND message.mailbox = ? AND message.generation = ? ORDER BY message.uid ASC LIMIT ?",
            )
            .bind(account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(MAILBOX_SNAPSHOT_REPLACEMENT_PUBLISH_BATCH_SIZE)
            .fetch_all(&mut **tx)
            .await?,
        };
        if rows.is_empty() {
            return Ok(());
        }
        for (uid, message_json) in &rows {
            let message: MailSummary = serde_json::from_str(message_json)
                .context("decode staged snapshot replacement message")?;
            if message.account_id != account_id || message.mailbox != mailbox || message.uid != *uid
            {
                return Err(anyhow!(
                    "staged replacement message does not match its snapshot UID namespace"
                ));
            }
            let staged: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?)",
            )
            .bind(account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(uid)
            .fetch_one(&mut **tx)
            .await?;
            if !staged {
                return Err(anyhow!(
                    "staged replacement message UID is absent from the finalized snapshot"
                ));
            }
            persist_message(tx, &message).await?;
        }
        after_uid = rows.last().map(|(uid, _)| *uid);
    }
}

async fn persist_message_with_flag_policy(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    message: &MailSummary,
    flag_policy: FlagUpdatePolicy,
) -> Result<()> {
    let suppressed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ? AND uid = ?)",
    )
    .bind(&message.account_id)
    .bind(&message.mailbox)
    .bind(message.uid)
    .fetch_one(&mut **tx)
    .await?;
    if suppressed {
        return Ok(());
    }
    let (provider_authoritative, expected_flags) = match flag_policy {
        FlagUpdatePolicy::ProviderAuthoritative => (true, None),
        FlagUpdatePolicy::CompareAndSwap(expected_flags) => (false, expected_flags),
    };
    let (expected_read, expected_flagged) = expected_flags.unwrap_or_default();
    sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, is_answered, is_draft, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET message_id=excluded.message_id, in_reply_to=excluded.in_reply_to, reference_ids=excluded.reference_ids, threading_scanned=1, recipient_headers_scanned=1, subject=excluded.subject, from_name=excluded.from_name, from_address=excluded.from_address, to_addresses=excluded.to_addresses, cc_addresses=excluded.cc_addresses, bcc_addresses=excluded.bcc_addresses, reply_to_addresses=excluded.reply_to_addresses, received_at=excluded.received_at, snippet=CASE WHEN excluded.content_state = 'complete' THEN excluded.snippet ELSE messages.snippet END, body_text=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_text ELSE messages.body_text END, body_html=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_html ELSE messages.body_html END, content_state=CASE WHEN messages.content_state = 'complete' THEN messages.content_state ELSE excluded.content_state END, unsubscribe_kind=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_kind ELSE messages.unsubscribe_kind END, unsubscribe_url=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_url ELSE messages.unsubscribe_url END, unsubscribe_scanned=CASE WHEN excluded.content_state = 'complete' THEN 1 ELSE messages.unsubscribe_scanned END, is_read=CASE WHEN ? OR (? AND messages.is_read = ? AND messages.is_flagged = ?) THEN excluded.is_read ELSE messages.is_read END, is_flagged=CASE WHEN ? OR (? AND messages.is_read = ? AND messages.is_flagged = ?) THEN excluded.is_flagged ELSE messages.is_flagged END, is_answered=excluded.is_answered, is_draft=excluded.is_draft, has_attachments=CASE WHEN excluded.content_state = 'complete' THEN excluded.has_attachments ELSE messages.has_attachments END, classification_confidence=CASE WHEN messages.classification_source = 'model' AND (messages.from_name IS NOT excluded.from_name OR messages.from_address != excluded.from_address OR messages.subject != excluded.subject OR messages.classification_signals != excluded.classification_signals OR (excluded.content_state = 'complete' AND (messages.snippet != excluded.snippet OR messages.body_text != excluded.body_text))) THEN NULL ELSE messages.classification_confidence END, classification_source=CASE WHEN messages.classification_source = 'model' AND (messages.from_name IS NOT excluded.from_name OR messages.from_address != excluded.from_address OR messages.subject != excluded.subject OR messages.classification_signals != excluded.classification_signals OR (excluded.content_state = 'complete' AND (messages.snippet != excluded.snippet OR messages.body_text != excluded.body_text))) THEN NULL ELSE messages.classification_source END, classification_signals=excluded.classification_signals")
        .bind(&message.id).bind(&message.account_id).bind(&message.mailbox).bind(message.uid)
        .bind(&message.message_id).bind(&message.in_reply_to).bind(&message.reference_ids).bind(&message.thread_id)
        .bind(&message.subject).bind(&message.from_name)
        .bind(&message.from_address).bind(&message.to_addresses)
        .bind(&message.cc_addresses).bind(&message.bcc_addresses).bind(&message.reply_to_addresses)
        .bind(message.received_at)
        // Message content is deliberately transient. The legacy columns stay
        // present for a backwards-compatible migration, but catalogue writes
        // can never repopulate them.
        .bind(&message.snippet).bind("").bind(Option::<String>::None).bind(&message.content_state)
        .bind(&message.unsubscribe_kind).bind(&message.unsubscribe_url)
        .bind(message.content_state == "complete").bind(message.is_read)
        .bind(message.is_flagged).bind(message.is_answered).bind(message.is_draft)
        .bind(message.has_attachments)
        .bind(&message.category).bind(message.classification_confidence)
        .bind(&message.classification_source).bind(&message.classification_signals)
        .bind(provider_authoritative)
        .bind(expected_flags.is_some())
        .bind(expected_read)
        .bind(expected_flagged)
        .bind(provider_authoritative)
        .bind(expected_flags.is_some())
        .bind(expected_read)
        .bind(expected_flagged)
        .execute(&mut **tx).await?;
    if message.mailbox == "Sent" || message.mailbox.starts_with("Sent::") {
        let recipients = [
            &message.to_addresses,
            &message.cc_addresses,
            &message.bcc_addresses,
        ]
        .into_iter()
        .flat_map(|header| crate::mail::parsed_header_mailboxes(header))
        .map(|address| address.to_lowercase())
        .collect::<HashSet<_>>();
        for recipient in recipients {
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO sent_correspondents(account_id, address) VALUES (?, ?)",
            )
            .bind(&message.account_id)
            .bind(&recipient)
            .execute(&mut **tx)
            .await?;
            if inserted.rows_affected() == 0 {
                continue;
            }
            // A new user-authored message is durable relationship evidence.
            // Reconsider only prior model decisions from that correspondent;
            // explicit user categories remain authoritative.
            sqlx::query("UPDATE messages SET classification_source = NULL, classification_confidence = NULL WHERE account_id = ? AND lower(from_address) = ? AND classification_source = 'model'")
                .bind(&message.account_id)
                .bind(recipient)
                .execute(&mut **tx)
                .await?;
        }
    }
    let effective_is_flagged = if matches!(flag_policy, FlagUpdatePolicy::CompareAndSwap(_)) {
        sqlx::query_scalar(
            "SELECT is_flagged FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
        )
        .bind(&message.account_id)
        .bind(&message.mailbox)
        .bind(message.uid)
        .fetch_one(&mut **tx)
        .await?
    } else {
        message.is_flagged
    };
    if effective_is_flagged && message.content_state == "complete" {
        sqlx::query("INSERT INTO starred_message_bodies(message_id, body_text, body_html, attachment_presentation_version, cached_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(message_id) DO UPDATE SET body_text=excluded.body_text, body_html=excluded.body_html, attachment_presentation_version=excluded.attachment_presentation_version, cached_at=excluded.cached_at")
            .bind(&message.id)
            .bind(&message.body_text)
            .bind(&message.body_html)
            .bind(ATTACHMENT_PRESENTATION_VERSION)
            .bind(Utc::now())
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id = ?")
            .bind(&message.id)
            .execute(&mut **tx)
            .await?;
        for attachment in message
            .attachments
            .iter()
            .filter(|attachment| attachment.attachment.presentation.is_downloadable())
        {
            sqlx::query("INSERT INTO starred_attachment_metadata(id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(&attachment.attachment.id)
                .bind(&message.id)
                .bind(&attachment.attachment.filename)
                .bind(&attachment.attachment.mime_type)
                .bind(attachment.attachment.size_bytes)
                .bind(attachment.attachment.is_inline)
                .bind(attachment.attachment.presentation)
                .bind(attachment.attachment.is_potentially_unsafe)
                .execute(&mut **tx)
                .await?;
        }
    } else if !effective_is_flagged {
        sqlx::query("DELETE FROM starred_message_bodies WHERE message_id = ?")
            .bind(&message.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id = ?")
            .bind(&message.id)
            .execute(&mut **tx)
            .await?;
    }
    // Provider catalogue fetches can supply BODYSTRUCTURE-derived filenames,
    // MIME types, sizes, and dispositions without attachment bytes. Replace
    // this message's durable search metadata in the same transaction as the
    // header upsert, so a later authoritative refresh removes stale files.
    // `AttachmentData::bytes` is intentionally never read or written here.
    sqlx::query("DELETE FROM message_attachment_catalogue WHERE message_id = ?")
        .bind(&message.id)
        .execute(&mut **tx)
        .await?;
    for attachment in &message.attachments {
        sqlx::query("INSERT OR REPLACE INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&message.id)
            .bind(&attachment.attachment.id)
            .bind(&attachment.attachment.filename)
            .bind(&attachment.attachment.mime_type)
            .bind(attachment.attachment.size_bytes)
            .bind(attachment.attachment.is_inline)
            .bind(attachment.attachment.presentation)
            .execute(&mut **tx)
            .await?;
    }
    // Attachment bytes are fetched from IMAP only when the user opens or
    // saves them. Never retain them in SQLite.
    sqlx::query("DELETE FROM attachments WHERE message_id = ?")
        .bind(&message.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn deserialize_account(data: &str) -> Result<Account> {
    let mut account: Account = serde_json::from_str(data).context("invalid stored account")?;
    account.ensure_account_name();
    Ok(account)
}

fn cache_entry_byte_size(
    body_text: &str,
    body_html: Option<&str>,
    unsubscribe_kind: Option<&str>,
    attachments_json: &str,
) -> Result<i64> {
    i64::try_from(
        body_text.len()
            + body_html.map_or(0, str::len)
            + unsubscribe_kind.map_or(0, str::len)
            + attachments_json.len(),
    )
    .context("cached message content is too large")
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("system random number generator failed"))?;
    Ok(bytes)
}

fn load_or_create_vault_key(path: &Path) -> Result<[u8; VAULT_KEY_LEN]> {
    match std::fs::read(path) {
        Ok(bytes) => parse_vault_key(path, &bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let key = random_bytes()?;
            let temporary_path = path.with_file_name(format!(
                ".{}.{}.tmp",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("vault"),
                uuid::Uuid::new_v4()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&temporary_path) {
                Ok(mut file) => {
                    file.write_all(&key)?;
                    file.sync_all()?;
                    drop(file);
                    match publish_vault_key(&temporary_path, path, true) {
                        Ok(()) => {
                            if temporary_path.exists() {
                                std::fs::remove_file(&temporary_path)?;
                            }
                            Ok(key)
                        }
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                            std::fs::remove_file(&temporary_path)?;
                            parse_vault_key(path, &std::fs::read(path)?)
                        }
                        Err(error) => {
                            let _ = std::fs::remove_file(&temporary_path);
                            Err(error.into())
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    parse_vault_key(path, &std::fs::read(path)?)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn publish_vault_key(
    temporary_path: &Path,
    path: &Path,
    try_hard_link: bool,
) -> std::io::Result<()> {
    publish_vault_key_with_stale_after(temporary_path, path, try_hard_link, Duration::from_secs(10))
}

fn publish_vault_key_with_stale_after(
    temporary_path: &Path,
    path: &Path,
    try_hard_link: bool,
    stale_after: Duration,
) -> std::io::Result<()> {
    publish_vault_key_with_stale_after_and_hook(
        temporary_path,
        path,
        try_hard_link,
        stale_after,
        || {},
    )
}

fn publish_vault_key_with_stale_after_and_hook<F>(
    temporary_path: &Path,
    path: &Path,
    try_hard_link: bool,
    stale_after: Duration,
    after_lock_acquired: F,
) -> std::io::Result<()>
where
    F: FnOnce(),
{
    let hard_link_result = if try_hard_link {
        std::fs::hard_link(temporary_path, path)
    } else {
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "hard links disabled for test",
        ))
    };
    match hard_link_result {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Err(error),
        Err(error)
            if !matches!(
                error.kind(),
                ErrorKind::Unsupported | ErrorKind::PermissionDenied | ErrorKind::Other
            ) =>
        {
            return Err(error);
        }
        Err(_) => {}
    }

    // Some valid profile locations (for example exFAT and network mounts) do
    // not support hard links. Serialize the fallback with a create-new lock,
    // then atomically rename the already-fsynced temporary file. Contenders
    // never observe partial key bytes and the winner never overwrites an
    // existing canonical key.
    let mut lock = VaultPublishLock::acquire(path, stale_after)?;
    after_lock_acquired();
    // Stop before checking ownership so the final token check observes stable
    // lock contents. A stale-lock taker moves the lock aside before acquiring
    // its own create-new lock, so a slow old publisher can never publish over
    // or clean up the replacement owner's work.
    lock.stop_heartbeat();
    let result = if !lock.is_owned()? {
        Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "lost ownership of vault key publication lock",
        ))
    } else if path.exists() {
        Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "vault key already published",
        ))
    } else if !lock.is_owned()? {
        // Keep this second check directly adjacent to the rename. The lock
        // serializes all Dakia publishers and this prevents a reclaimed old
        // owner from replacing a newer canonical key in the fallback path.
        Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "lost ownership of vault key publication lock",
        ))
    } else {
        // `temporary_path` was fsynced before acquiring the lock. The lock is
        // create-new and checked immediately above, so fallback publishers
        // retain the same no-replace canonical semantics as the hard-link
        // path while still supporting filesystems without hard links.
        std::fs::rename(temporary_path, path)
    };
    let _ = lock.release_if_owned();
    result
}

struct VaultPublishLock {
    path: PathBuf,
    token: String,
    heartbeat: Option<VaultPublishHeartbeat>,
}

struct VaultPublishHeartbeat {
    stop: std::sync::mpsc::Sender<()>,
    worker: std::thread::JoinHandle<()>,
}

impl VaultPublishLock {
    fn acquire(canonical_path: &Path, stale_after: Duration) -> std::io::Result<Self> {
        let path = vault_publish_lock_path(canonical_path);
        let deadline = Instant::now() + stale_after;
        let token = uuid::Uuid::new_v4().to_string();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(token.as_bytes())?;
                    file.sync_all()?;
                    drop(file);
                    return Ok(Self {
                        path: path.clone(),
                        token: token.clone(),
                        heartbeat: VaultPublishHeartbeat::start(&path, &token, stale_after),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if canonical_path.exists() {
                        return Err(std::io::Error::new(
                            ErrorKind::AlreadyExists,
                            "vault key already published",
                        ));
                    }
                    if vault_publish_lock_is_stale(&path, stale_after) {
                        // Move, rather than unlink, a stale lock. A live
                        // holder that was paused can then detect that its
                        // token no longer owns `path` before final publish or
                        // cleanup; it cannot delete the contender's lock.
                        match move_stale_vault_publish_lock(&path) {
                            Ok(()) => continue,
                            Err(ref error) if error.kind() == ErrorKind::NotFound => continue,
                            Err(error) => return Err(error),
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            ErrorKind::TimedOut,
                            "timed out waiting for vault key publication",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn is_owned(&self) -> std::io::Result<bool> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(contents == self.token),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn stop_heartbeat(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.stop.send(());
            let _ = heartbeat.worker.join();
        }
    }

    fn release_if_owned(&mut self) -> std::io::Result<()> {
        self.stop_heartbeat();
        if self.is_owned()? {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(ref error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for VaultPublishLock {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

impl VaultPublishHeartbeat {
    fn start(path: &Path, token: &str, stale_after: Duration) -> Option<Self> {
        if stale_after.is_zero() {
            return None;
        }
        let interval = stale_after
            .checked_div(3)
            .unwrap_or(stale_after)
            .max(Duration::from_millis(1))
            .min(Duration::from_secs(1));
        let path = path.to_owned();
        let token = token.to_owned();
        let (stop, worker_stop) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || loop {
            match worker_stop.recv_timeout(interval) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    refresh_vault_publish_lock(&path, &token);
                }
            }
        });
        Some(Self { stop, worker })
    }
}

fn vault_publish_lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.publish.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("vault")
    ))
}

fn vault_publish_lock_is_stale(path: &Path, stale_after: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= stale_after)
}

fn move_stale_vault_publish_lock(path: &Path) -> std::io::Result<()> {
    let stale_path = path.with_file_name(format!(
        ".{}.stale.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("vault"),
        uuid::Uuid::new_v4()
    ));
    std::fs::rename(path, &stale_path)?;
    match std::fs::remove_file(&stale_path) {
        Ok(()) => Ok(()),
        Err(ref error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn refresh_vault_publish_lock(path: &Path, token: &str) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_ok() && contents == token {
        let _ = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
    }
}

fn parse_vault_key(path: &Path, bytes: &[u8]) -> Result<[u8; VAULT_KEY_LEN]> {
    let key: [u8; VAULT_KEY_LEN] = bytes.try_into().map_err(|_| {
        anyhow!(
            "credential vault key at {} has an invalid length",
            path.display()
        )
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(key)
}

fn encryption_key(key: &[u8; VAULT_KEY_LEN]) -> Result<LessSafeKey> {
    Ok(LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key)
            .map_err(|_| anyhow!("credential vault key is invalid"))?,
    ))
}

fn encrypt_secret(
    key: &[u8; VAULT_KEY_LEN],
    nonce: [u8; VAULT_NONCE_LEN],
    name: &str,
    secret: &str,
) -> Result<Vec<u8>> {
    let mut ciphertext = secret.as_bytes().to_vec();
    encryption_key(key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(name.as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| anyhow!("could not encrypt credential"))?;
    Ok(ciphertext)
}

fn decrypt_secret(
    key: &[u8; VAULT_KEY_LEN],
    nonce: &[u8],
    name: &str,
    mut ciphertext: Vec<u8>,
) -> Result<String> {
    let nonce: [u8; VAULT_NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| anyhow!("stored credential nonce has an invalid length"))?;
    let plaintext = encryption_key(key)?
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(name.as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| anyhow!("stored credential could not be decrypted"))?;
    String::from_utf8(plaintext.to_vec()).context("stored credential is not valid UTF-8")
}

/// Returns every folder term under positive Boolean polarity. A system row is
/// allowed only when one of these scopes matches its visible family or remote
/// alias; a plain text OR branch can never opt it in.
fn positive_folder_scopes(node: &SearchNode) -> Vec<FolderScope> {
    let mut scopes = Vec::new();
    collect_positive_folder_scopes(node, true, &mut scopes);
    scopes
}

fn collect_positive_folder_scopes(
    node: &SearchNode,
    positive: bool,
    scopes: &mut Vec<FolderScope>,
) {
    match node {
        SearchNode::Term(SearchTerm::Folder(scope)) if positive => scopes.push(scope.clone()),
        SearchNode::And(nodes) | SearchNode::Or(nodes) => {
            for node in nodes {
                collect_positive_folder_scopes(node, positive, scopes);
            }
        }
        SearchNode::Not(node) => collect_positive_folder_scopes(node, !positive, scopes),
        _ => {}
    }
}

fn is_system_mailbox(mailbox: &str) -> bool {
    matches!(mailbox_family(mailbox), family if family.eq_ignore_ascii_case("Spam") || family.eq_ignore_ascii_case("Trash"))
}

fn folder_scope_matches_system_mailbox(scope: &FolderScope, mailbox: &str) -> bool {
    let (family, remote) = mailbox.split_once("::").unwrap_or((mailbox, mailbox));
    match scope {
        FolderScope::All => true,
        FolderScope::Exact(folder) => {
            let folder = normalize_search_text(folder);
            !folder.contains("::")
                && (folder == normalize_search_text(family)
                    || folder == normalize_search_text(remote))
        }
        FolderScope::Descendants(folder) => {
            let folder = normalize_search_text(folder);
            let remote = normalize_search_text(remote);
            !folder.contains("::")
                && (remote == folder
                    || remote
                        .strip_prefix(&folder)
                        .is_some_and(|suffix| suffix.starts_with('/')))
        }
    }
}

fn expression_contains_no_attachment(node: &SearchNode) -> bool {
    match node {
        SearchNode::MatchAll => false,
        SearchNode::Term(SearchTerm::Attachment(AttachmentPredicate::HasNoAttachment)) => true,
        SearchNode::Term(_) => false,
        SearchNode::And(nodes) | SearchNode::Or(nodes) => {
            nodes.iter().any(expression_contains_no_attachment)
        }
        SearchNode::Not(node) => expression_contains_no_attachment(node),
    }
}

fn search_message_cursor(message: &MailSummary) -> MailCursor {
    MailCursor {
        received_at: message.received_at,
        id: message.id.clone(),
    }
}

fn search_match_evidence(matches: &[MailSummary]) -> SearchMatchEvidence {
    let primary = matches.first();
    SearchMatchEvidence {
        primary_message_id: primary.map(|message| message.id.clone()),
        matched_message_ids: matches.iter().map(|message| message.id.clone()).collect(),
        match_count: u32::try_from(matches.len()).unwrap_or(u32::MAX),
        excerpt: primary.and_then(safe_search_excerpt),
    }
}

/// Keeps excerpts display-safe even when an untrusted text part contains
/// terminal controls or unusually long whitespace runs. This deliberately
/// receives the evaluated match row, not the conversation hydration row,
/// whose body is intentionally blank for list performance.
fn safe_search_excerpt(message: &MailSummary) -> Option<String> {
    let source = [&message.body_text, &message.snippet, &message.subject]
        .into_iter()
        .find(|text| !text.trim().is_empty())?;
    let normalized = source
        .chars()
        .filter_map(|character| {
            if character.is_control() && !character.is_whitespace() {
                None
            } else if character.is_whitespace() {
                Some(' ')
            } else {
                Some(character)
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return None;
    }
    const MAX_EXCERPT_CHARS: usize = 240;
    let mut excerpt = normalized
        .chars()
        .take(MAX_EXCERPT_CHARS)
        .collect::<String>();
    if normalized.chars().nth(MAX_EXCERPT_CHARS).is_some() {
        excerpt.push('…');
    }
    Some(excerpt)
}

fn is_sqlite_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| is_sqlite_busy_message(&cause.to_string()))
}

fn is_sqlite_busy_message(message: &str) -> bool {
    message.contains("database is locked") || message.contains("database is busy")
}

fn is_sqlite_migration_race(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("duplicate column name") || message.contains("vtable constructor failed")
    })
}

fn is_opaque_mailbox_storage_identity(value: &str) -> bool {
    let special = value
        .split_once("::")
        .and_then(|(family, encoded)| {
            special_mailbox_family(family).and_then(|_| {
                encoded
                    .strip_prefix("@dakia-special-v1:")
                    .filter(|encoded| URL_SAFE_NO_PAD.decode(encoded).is_ok())
            })
        })
        .is_some();
    if special {
        return true;
    }
    let Some(encoded) = value.strip_prefix("Mailbox::@dakia-mailbox-v1:") else {
        return false;
    };
    let Some((remote, display)) = encoded.split_once(':') else {
        return false;
    };
    URL_SAFE_NO_PAD.decode(remote).is_ok() && URL_SAFE_NO_PAD.decode(display).is_ok()
}

fn special_mailbox_family(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "inbox" => Some("INBOX"),
        "sent" => Some("Sent"),
        "drafts" => Some("Drafts"),
        "archive" => Some("Archive"),
        "spam" => Some("Spam"),
        "trash" => Some("Trash"),
        _ => None,
    }
}

fn special_mailbox_family_from_use(value: Option<&str>) -> Option<&'static str> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "\\inbox" => Some("INBOX"),
        "\\sent" => Some("Sent"),
        "\\drafts" => Some("Drafts"),
        "\\archive" | "\\all" => Some("Archive"),
        "\\junk" | "\\spam" => Some("Spam"),
        "\\trash" => Some("Trash"),
        _ => None,
    }
}

fn default_special_mailbox_remote(account: &Account, family: &str) -> Option<String> {
    match (account.provider_id.as_str(), family) {
        (_, "INBOX") => Some("INBOX".into()),
        ("gmail", "Sent") => Some("[Gmail]/Sent Mail".into()),
        ("gmail", "Drafts") => Some("[Gmail]/Drafts".into()),
        ("gmail", "Archive") => Some(account.archive_mailbox.clone()),
        ("gmail", "Spam") => Some(account.spam_mailbox.clone()),
        ("gmail", "Trash") => Some("[Gmail]/Trash".into()),
        (_, "Sent") => Some("Sent".into()),
        (_, "Drafts") => Some("Drafts".into()),
        (_, "Archive") => Some(account.archive_mailbox.clone()),
        (_, "Spam") => Some(account.spam_mailbox.clone()),
        (_, "Trash") => Some("Trash".into()),
        _ => None,
    }
}

/// Maps exactly one historical mailbox locator using a current selectable
/// mailbox row.  The tuple carries `(selectable_id, new_storage_locator)`.
fn opaque_mailbox_migration_target(
    account: &Account,
    legacy_mailbox: &str,
    catalogue_state: Option<(&str, i64)>,
    candidate: &(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ),
) -> Option<(String, String)> {
    let (id, _account_id, remote, _local, delimiter, special_use, uid_validity) = candidate;
    if let Some((state_remote, state_uid_validity)) = catalogue_state {
        if state_remote != remote {
            return None;
        }
        if uid_validity.is_some_and(|value| value != state_uid_validity) {
            return None;
        }
    }
    if let Some(family) = special_mailbox_family_from_use(special_use.as_deref()) {
        // Default special folders deliberately retain their established local
        // namespace.  This keeps existing standard INBOX/Sent rows stable;
        // only a resolved alias receives a role-qualified opaque key.
        if legacy_mailbox == family
            && default_special_mailbox_remote(account, family).as_deref() == Some(remote.as_str())
        {
            return Some((id.clone(), family.to_owned()));
        }
        if legacy_mailbox == format!("{family}::{remote}") {
            return Some((id.clone(), special_mailbox_storage_identity(family, remote)));
        }
        return None;
    }
    if special_mailbox_family(legacy_mailbox).is_some_and(|family| {
        default_special_mailbox_remote(account, family).as_deref() == Some(remote.as_str())
    }) {
        // Old profiles did not always persist IMAP special-use flags.  The
        // configured default raw path still proves a standard local family,
        // so retain its compact established key instead of turning INBOX or
        // Sent into a generic opaque namespace.
        return Some((id.clone(), legacy_mailbox.to_owned()));
    }
    // A normal mailbox is only mapped from its exact raw provider path.  A
    // display/local path may differ by delimiter, whitespace, modified UTF-7,
    // case, or diacritics and is therefore not identity evidence.
    (legacy_mailbox == remote).then(|| {
        (
            id.clone(),
            generic_mailbox_storage_identity(
                remote,
                &opaque_mailbox_display_path(remote, delimiter.as_deref()),
            ),
        )
    })
}

fn opaque_mailbox_display_path(remote: &str, delimiter: Option<&str>) -> String {
    delimiter
        .filter(|delimiter| !delimiter.is_empty())
        .map(|delimiter| {
            remote
                .split(delimiter)
                .map(|component| {
                    display_imap_mailbox_name(component)
                        .replace('%', "%25")
                        .replace('/', "%2F")
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_else(|| {
            display_imap_mailbox_name(remote)
                .replace('%', "%25")
                .replace('/', "%2F")
        })
}

fn selectable_mailbox_storage_locator(mailbox: &SelectableMailbox) -> String {
    let special_family = mailbox
        .special_use
        .as_deref()
        .map(str::to_ascii_lowercase)
        .and_then(|special| match special.as_str() {
            "\\inbox" => Some("INBOX"),
            "\\sent" => Some("Sent"),
            "\\drafts" => Some("Drafts"),
            "\\archive" | "\\all" => Some("Archive"),
            "\\junk" | "\\spam" => Some("Spam"),
            "\\trash" => Some("Trash"),
            _ => None,
        });
    special_family
        .map(|family| special_mailbox_storage_identity(family, &mailbox.remote_path))
        .unwrap_or_else(|| {
            generic_mailbox_storage_identity(
                &mailbox.remote_path,
                &opaque_mailbox_display_path(
                    &mailbox.remote_path,
                    mailbox.hierarchy_delimiter.as_deref(),
                ),
            )
        })
}

async fn note_unresolved_opaque_mailbox_locator_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    mailbox: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO opaque_mailbox_storage_identity_unresolved(account_id, mailbox, noted_at) VALUES (?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET noted_at = excluded.noted_at",
    )
    .bind(account_id)
    .bind(mailbox)
    .bind(Utc::now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn opaque_mailbox_target_is_safe_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    legacy_mailbox: &str,
    target: &str,
) -> Result<bool> {
    let target_messages: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE account_id = ? AND mailbox = ?)",
    )
    .bind(account_id)
    .bind(target)
    .fetch_one(&mut **tx)
    .await?;
    let source_catalogue: Option<i64> = sqlx::query_scalar(
        "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(legacy_mailbox)
    .fetch_optional(&mut **tx)
    .await?;
    let target_catalogue: Option<i64> = sqlx::query_scalar(
        "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(target)
    .fetch_optional(&mut **tx)
    .await?;
    if source_catalogue
        .zip(target_catalogue)
        .is_some_and(|(left, right)| left != right)
    {
        return Ok(false);
    }
    // A provider may publish a new opaque row while this background upgrade
    // is between batches.  Merging it is safe only when both namespaces have
    // an authoritative, equal UIDVALIDITY. Without that proof, a reused UID
    // could name unrelated mail and must wait for a clean recatalogue.
    if target_messages
        && (source_catalogue.is_none()
            || target_catalogue.is_none()
            || source_catalogue != target_catalogue)
    {
        return Ok(false);
    }
    let source_sync: Option<i64> = sqlx::query_scalar(
        "SELECT uid_validity FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(legacy_mailbox)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let target_sync: Option<i64> = sqlx::query_scalar(
        "SELECT uid_validity FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(target)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    Ok(source_sync
        .zip(target_sync)
        .is_none_or(|(left, right)| left == right))
}

async fn migrate_mailbox_catalogue_state_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    legacy_mailbox: &str,
    target: &str,
) -> Result<()> {
    // This deliberately copies rather than moves the source record. A
    // mailbox can span several bounded transactions, and its legacy state is
    // the UIDVALIDITY proof needed before the next transaction can safely
    // merge an already-published opaque row. The caller deletes the source
    // only after the final legacy message has moved.
    let source: Option<(String, i64, i64, i64, String)> = sqlx::query_as(
        "SELECT remote_name, uid_validity, remote_total, historical_complete, updated_at \
         FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(legacy_mailbox)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((remote_name, uid_validity, remote_total, historical_complete, updated_at)) = source
    else {
        return Ok(());
    };
    sqlx::query(
        "INSERT OR IGNORE INTO mailbox_catalog_state(\
           account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at\
         ) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(account_id)
    .bind(target)
    .bind(remote_name)
    .bind(uid_validity)
    .bind(remote_total)
    .bind(historical_complete)
    .bind(&updated_at)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE mailbox_catalog_state SET \
           remote_total = MAX(remote_total, ?), \
           historical_complete = MAX(historical_complete, ?), \
           updated_at = MAX(updated_at, ?) \
         WHERE account_id = ? AND mailbox = ?",
    )
    .bind(remote_total)
    .bind(historical_complete)
    .bind(updated_at)
    .bind(account_id)
    .bind(target)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn migrate_mailbox_sync_state_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    legacy_mailbox: &str,
    target: &str,
) -> Result<()> {
    // Keep the old sync record for the same reason as catalogue state above:
    // it is authoritative identity evidence until the source mailbox is
    // empty. It is finalized by the caller with the other source markers.
    let source: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT initialized_at, highest_uid, uid_validity \
         FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?",
    )
    .bind(account_id)
    .bind(legacy_mailbox)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((initialized_at, highest_uid, uid_validity)) = source else {
        return Ok(());
    };
    sqlx::query(
        "INSERT OR IGNORE INTO mailbox_sync_state(\
           account_id, mailbox, initialized_at, highest_uid, uid_validity\
         ) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(account_id)
    .bind(target)
    .bind(&initialized_at)
    .bind(highest_uid)
    .bind(uid_validity)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE mailbox_sync_state SET \
           highest_uid = COALESCE(MAX(highest_uid, ?), highest_uid, ?), \
           initialized_at = MAX(initialized_at, ?) \
         WHERE account_id = ? AND mailbox = ?",
    )
    .bind(highest_uid)
    .bind(highest_uid)
    .bind(initialized_at)
    .bind(account_id)
    .bind(target)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn rewrite_attachment_identity_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    old_message_id: &str,
    new_message_id: &str,
) -> Result<()> {
    for table in ["attachments", "starred_attachment_metadata"] {
        sqlx::query(&format!(
            "UPDATE OR IGNORE {table} SET id = ? || substr(id, length(?) + 1) \
             WHERE substr(id, 1, length(?) + 1) = ? || ':'"
        ))
        .bind(new_message_id)
        .bind(old_message_id)
        .bind(old_message_id)
        .bind(old_message_id)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "UPDATE message_attachment_catalogue \
         SET attachment_id = ? || substr(attachment_id, length(?) + 1) \
         WHERE substr(attachment_id, 1, length(?) + 1) = ? || ':'",
    )
    .bind(new_message_id)
    .bind(old_message_id)
    .bind(old_message_id)
    .bind(old_message_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn rewrite_cached_attachment_identity_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    new_message_id: &str,
    old_message_id: &str,
) -> Result<()> {
    let cached: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT body_text, body_html, unsubscribe_kind, attachments_json FROM message_content_cache WHERE message_id = ?",
    )
    .bind(new_message_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((body_text, body_html, unsubscribe_kind, attachments_json)) = cached else {
        return Ok(());
    };
    let Ok(mut attachments) = serde_json::from_str::<Vec<Attachment>>(&attachments_json) else {
        // Preserve the original bytes.  The reader already handles this
        // legacy corruption by discarding the cache and fetching again.
        return Ok(());
    };
    for attachment in &mut attachments {
        attachment.message_id = new_message_id.to_owned();
        if let Some(suffix) = attachment
            .id
            .strip_prefix(old_message_id)
            .and_then(|value| value.strip_prefix(':'))
        {
            attachment.id = format!("{new_message_id}:{suffix}");
        }
    }
    let rewritten = serde_json::to_string(&attachments)?;
    let byte_size = cache_entry_byte_size(
        &body_text,
        body_html.as_deref(),
        unsubscribe_kind.as_deref(),
        &rewritten,
    )?;
    sqlx::query(
        "UPDATE message_content_cache SET attachments_json = ?, byte_size = ? WHERE message_id = ?",
    )
    .bind(rewritten)
    .bind(byte_size)
    .bind(new_message_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Coalesces a legacy locator into a provider-published opaque row after the
/// caller has proved account, raw mailbox and UIDVALIDITY equivalence. The
/// published message remains authoritative for flags and header metadata;
/// local complete bodies and attachment metadata move across only when the
/// new row does not already have them.
async fn merge_legacy_message_into_published_opaque_row_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    old_message_id: &str,
    new_message_id: &str,
) -> Result<()> {
    rewrite_attachment_identity_in_tx(tx, old_message_id, new_message_id).await?;
    for statement in [
        "INSERT OR IGNORE INTO message_mailbox_memberships(message_id, mailbox_id, account_id) SELECT ?, mailbox_id, account_id FROM message_mailbox_memberships WHERE message_id = ?",
        "INSERT OR IGNORE INTO message_content_cache(message_id, content_state, body_text, body_html, unsubscribe_kind, attachments_json, byte_size, last_accessed) SELECT ?, content_state, body_text, body_html, unsubscribe_kind, attachments_json, byte_size, last_accessed FROM message_content_cache WHERE message_id = ?",
        "INSERT OR IGNORE INTO message_search_body_text(message_id, body_text, byte_size, last_indexed) SELECT ?, body_text, byte_size, last_indexed FROM message_search_body_text WHERE message_id = ?",
        "INSERT OR IGNORE INTO starred_message_bodies(message_id, body_text, body_html, attachment_presentation_version, cached_at) SELECT ?, body_text, body_html, attachment_presentation_version, cached_at FROM starred_message_bodies WHERE message_id = ?",
        "INSERT OR IGNORE INTO attachments(id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe, data) SELECT id, ?, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe, data FROM attachments WHERE message_id = ?",
        "INSERT OR IGNORE INTO starred_attachment_metadata(id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe) SELECT id, ?, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe FROM starred_attachment_metadata WHERE message_id = ?",
        "INSERT OR IGNORE INTO message_attachment_catalogue(message_id, attachment_id, filename, mime_type, size_bytes, is_inline, presentation) SELECT ?, attachment_id, filename, mime_type, size_bytes, is_inline, presentation FROM message_attachment_catalogue WHERE message_id = ?",
    ] {
        sqlx::query(statement)
            .bind(new_message_id)
            .bind(old_message_id)
            .execute(&mut **tx)
            .await?;
    }
    for table in [
        "contacted_people_backfill_messages",
        "contacted_people_legacy_unresolved_sources",
    ] {
        sqlx::query(&format!(
            "INSERT OR IGNORE INTO {table}(account_id, message_id) SELECT account_id, ? FROM {table} WHERE account_id = ? AND message_id = ?"
        ))
        .bind(new_message_id)
        .bind(account_id)
        .bind(old_message_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE account_id = ? AND message_id = ?"
        ))
        .bind(account_id)
        .bind(old_message_id)
        .execute(&mut **tx)
        .await?;
    }
    rewrite_cached_attachment_identity_in_tx(tx, new_message_id, old_message_id).await?;
    // Cascades remove any source dependency that was not transferred above.
    // This leaves the provider row's current flags/header metadata intact.
    sqlx::query("DELETE FROM messages WHERE id = ? AND account_id = ?")
        .bind(old_message_id)
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn save_account_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account: &Account,
) -> Result<bool> {
    let account_id = account.id.to_string();
    let deleted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
    )
    .bind(&account_id)
    .fetch_one(&mut **tx)
    .await?;
    if deleted {
        return Err(anyhow!("account was removed"));
    }
    let existing: Option<String> = sqlx::query_scalar("SELECT data FROM accounts WHERE id = ?")
        .bind(&account_id)
        .fetch_optional(&mut **tx)
        .await?;
    let active_membership_changed = existing
        .as_deref()
        .and_then(|data| deserialize_account(data).ok())
        .is_some_and(|previous| previous.enabled != account.enabled);
    sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET email=excluded.email, data=excluded.data")
        .bind(&account_id)
        .bind(&account.email)
        .bind(serde_json::to_string(account)?)
        .bind(account.created_at)
        .execute(&mut **tx)
        .await?;
    if existing.is_some() {
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) VALUES (?, 0)")
            .bind(&account_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE account_search_generations SET generation = generation + 1 WHERE account_id = ?")
            .bind(&account_id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("INSERT OR IGNORE INTO account_search_generations(account_id, generation) VALUES (?, 0)")
            .bind(&account_id)
            .execute(&mut **tx)
            .await?;
    }
    if active_membership_changed {
        sqlx::query("DELETE FROM app_meta WHERE key IN (?, ?)")
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY)
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY)
            .execute(&mut **tx)
            .await?;
    }
    Ok(active_membership_changed)
}

async fn save_mail_rebuild_job_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    job: &MailRebuildJob,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO mail_rebuild_jobs(account_id, phase, completed, total, reset_before_sync, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id) DO UPDATE SET phase=excluded.phase, completed=excluded.completed, total=excluded.total, reset_before_sync=excluded.reset_before_sync, updated_at=excluded.updated_at",
    )
    .bind(job.account_id.to_string())
    .bind(&job.phase)
    .bind(i64::try_from(job.completed)?)
    .bind(job.total.map(i64::try_from).transpose()?)
    .bind(job.reset_before_sync)
    .bind(Utc::now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub fn stable_message_id(account_id: AccountId, mailbox: &str, uid: u32) -> String {
    // UUID v4 is used for accounts; deriving a stable ID avoids duplicates
    // during resync. The mailbox is opaque provider data, so delimiter
    // replacement is not injective (`A:B` and `A_B` used to collide). Keep
    // the established compact form for ordinary legacy paths and encode every
    // path that could have collided with it. Existing rows keep their stored
    // IDs through the `(account_id, mailbox, uid)` upsert key; new ambiguous
    // paths receive the collision-free v2 form.
    if mailbox.contains(':') {
        format!(
            "{account_id}:v2:{}:{uid}",
            URL_SAFE_NO_PAD.encode(mailbox.as_bytes())
        )
    } else {
        format!("{account_id}:{mailbox}:{uid}")
    }
}

#[cfg(test)]
// The snapshot implementation is intentionally kept beside the feature's
// storage helpers below this long-standing test module. Keep this narrowly
// scoped until the module is split into its own test file.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_contention_helpers_classify_synthetic_error_chains() {
        assert!(is_sqlite_busy_message("database is locked"));
        assert!(is_sqlite_busy_message("database is busy"));
        assert!(!is_sqlite_busy_message("disk I/O error"));

        let locked = anyhow!("database is locked").context("opening the store");
        assert!(is_sqlite_busy(&locked));
        assert!(!is_sqlite_busy(&anyhow!("disk I/O error")));

        let migration_race = anyhow!("duplicate column name: indexed_at").context("migrating");
        assert!(is_sqlite_migration_race(&migration_race));
        assert!(!is_sqlite_migration_race(&anyhow!(
            "no such column: indexed_at"
        )));
    }

    fn legacy_oauth_account(id: uuid::Uuid) -> Account {
        let mut account = account_with_id(id, "legacy@gmail.com");
        account.provider_id = "gmail".into();
        account.auth = AccountAuth::OAuth2 {
            username: "legacy@gmail.com".into(),
            provider: "gmail".into(),
            access_token_expires_at: None,
        };
        account
    }

    #[tokio::test]
    async fn account_and_credential_conversion_commits_together_or_not_at_all() {
        let store = Store::in_memory().await.unwrap();
        let original = legacy_oauth_account(uuid::Uuid::new_v4());
        let secret_name = format!(
            "dev.dakia.mail:{}:{}",
            original.id,
            original.auth.username()
        );
        store
            .save_account_with_secret(&original, &secret_name, "legacy-token-json")
            .await
            .unwrap();

        let mut converted = original.clone();
        converted.auth = AccountAuth::Password {
            username: original.auth.username().into(),
        };
        store
            .save_account_with_secret(&converted, &secret_name, "new-app-password")
            .await
            .unwrap();
        assert!(matches!(
            store.account(original.id).await.unwrap().unwrap().auth,
            AccountAuth::Password { .. }
        ));
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("new-app-password")
        );

        let mut rollback_candidate = converted.clone();
        rollback_candidate.auth = AccountAuth::OAuth2 {
            username: original.auth.username().into(),
            provider: "gmail".into(),
            access_token_expires_at: None,
        };
        sqlx::query(
            "CREATE TRIGGER reject_account_update BEFORE UPDATE ON accounts BEGIN SELECT RAISE(ABORT, 'forced account update failure'); END",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let error = store
            .save_account_with_secret(&rollback_candidate, &secret_name, "replacement-token-json")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("forced account update failure"));
        assert!(matches!(
            store.account(original.id).await.unwrap().unwrap().auth,
            AccountAuth::Password { .. }
        ));
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("new-app-password")
        );
    }

    #[tokio::test]
    async fn stale_oauth_refresh_cannot_replace_a_converted_app_password() {
        let store = Store::in_memory().await.unwrap();
        let original = legacy_oauth_account(uuid::Uuid::new_v4());
        let secret_name = format!(
            "dev.dakia.mail:{}:{}",
            original.id,
            original.auth.username()
        );
        store
            .save_account_with_secret(&original, &secret_name, "legacy-token-json")
            .await
            .unwrap();
        let mut converted = original.clone();
        converted.auth = AccountAuth::Password {
            username: original.auth.username().into(),
        };
        store
            .save_account_with_secret(&converted, &secret_name, "new-app-password")
            .await
            .unwrap();

        assert!(!store
            .set_oauth_secret_if_current(original.id, &secret_name, "stale-refreshed-token")
            .await
            .unwrap());
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("new-app-password")
        );
    }

    #[test]
    fn parses_only_complete_canonical_message_id_lists() {
        assert_eq!(
            parse_message_ids(" <Root@Example.Test>\t<Reply@example.test> "),
            vec!["root@example.test", "reply@example.test"]
        );
        assert!(parse_message_ids("<missing-close@example.test").is_empty());
        assert!(parse_message_ids("prefix <id@example.test>").is_empty());
    }

    #[test]
    fn stable_message_ids_keep_distinct_opaque_mailbox_paths_distinct() {
        let account_id = uuid::Uuid::new_v4();
        assert_ne!(
            stable_message_id(account_id, "Projects:2026", 7),
            stable_message_id(account_id, "Projects_2026", 7),
            "provider mailbox punctuation must not collapse into an ID collision"
        );
        assert_eq!(
            stable_message_id(account_id, "INBOX", 7),
            format!("{account_id}:INBOX:7"),
            "ordinary legacy IDs remain stable"
        );
    }

    #[tokio::test]
    async fn opaque_mailbox_locator_migration_uses_catalogue_identity_and_preserves_dependents() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "opaque-locator@example.test".into(),
            display_name: "Opaque locator".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let account_key = account.id.to_string();

        // A resolved Sent alias and an ordinary provider mailbox whose raw
        // name starts with `Sent::` are both legal.  The catalogue state's
        // raw remote path proves which interpretation each old locator used.
        let sent_alias = "Sent::Sént Items";
        let sent = store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Sént Items".into(),
                    local_path: Some(sent_alias.into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(17),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let literal_name = "Sent::Literal";
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Literal".into(),
                    local_path: Some(literal_name.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(23),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: literal_name.into(),
                    local_path: Some(literal_name.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(23),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let dotted_name = " Projects.Été/2026 ";
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: dotted_name.into(),
                    local_path: Some(dotted_name.into()),
                    hierarchy_delimiter: Some(".".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(31),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();

        let mut sent_message = message("Sent alias", "cached body");
        sent_message.id = stable_message_id(account.id, sent_alias, 7);
        sent_message.account_id = account_key.clone();
        sent_message.thread_id = sent_message.id.clone();
        sent_message.mailbox = sent_alias.into();
        sent_message.uid = 7;
        let mut literal_message = message("Literal", "body");
        literal_message.id = stable_message_id(account.id, literal_name, 8);
        literal_message.account_id = account_key.clone();
        literal_message.thread_id = literal_message.id.clone();
        literal_message.mailbox = literal_name.into();
        literal_message.uid = 8;
        let mut dotted_message = message("Dots", "body");
        dotted_message.id = stable_message_id(account.id, dotted_name, 9);
        dotted_message.account_id = account_key.clone();
        dotted_message.thread_id = dotted_message.id.clone();
        dotted_message.mailbox = dotted_name.into();
        dotted_message.uid = 9;
        store
            .upsert_messages(&[
                sent_message.clone(),
                literal_message.clone(),
                dotted_message.clone(),
            ])
            .await
            .unwrap();
        store
            .set_message_mailbox_memberships(
                account.id,
                &sent_message.id,
                std::slice::from_ref(&sent.id),
            )
            .await
            .unwrap();
        let old_attachment_id = format!("{}:mime-v1:1", sent_message.id);
        store
            .cache_message_content(
                &sent_message.id,
                false,
                CachedMessageContent {
                    body_text: "cached body".into(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: vec![Attachment {
                        id: old_attachment_id.clone(),
                        message_id: sent_message.id.clone(),
                        filename: "invoice.pdf".into(),
                        mime_type: "application/pdf".into(),
                        size_bytes: 3,
                        is_inline: false,
                        presentation: AttachmentPresentation::Downloadable,
                        is_potentially_unsafe: false,
                    }],
                },
            )
            .await
            .unwrap();
        store
            .cache_search_body_text(&sent_message.id, "provider body")
            .await
            .unwrap();
        sqlx::query("INSERT INTO attachments(id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe, data) VALUES (?, ?, 'invoice.pdf', 'application/pdf', 3, 0, 'downloadable', 0, X'00')")
            .bind(&old_attachment_id)
            .bind(&sent_message.id)
            .execute(&store.pool)
            .await
            .unwrap();
        for (mailbox, remote_name, uid_validity) in [
            (sent_alias, "Sént Items", 17_i64),
            (literal_name, literal_name, 23_i64),
            (dotted_name, dotted_name, 31_i64),
        ] {
            sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, ?, ?, 9, 1, ?)")
                .bind(&account_key)
                .bind(mailbox)
                .bind(remote_name)
                .bind(uid_validity)
                .bind(Utc::now())
                .execute(&store.pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid, uid_validity) VALUES (?, ?, ?, 9, ?)")
                .bind(&account_key)
                .bind(mailbox)
                .bind(Utc::now())
                .bind(uid_validity)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, ?, 7, ?)")
            .bind(&account_key)
            .bind(sent_alias)
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)",
        )
        .bind(&account_key)
        .bind(&sent_message.id)
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO contacted_people_backfill_sources(account_id, mailbox, uid_validity, uid) VALUES (?, ?, 17, 7)")
            .bind(&account_key)
            .bind(sent_alias)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();

        let sent_target = special_mailbox_storage_identity("Sent", "Sént Items");
        let literal_target = generic_mailbox_storage_identity("Sent::Literal", "Sent::Literal");
        let dotted_target = generic_mailbox_storage_identity(dotted_name, " Projects/Été%2F2026 ");
        let sent_new_id = stable_message_id(account.id, &sent_target, 7);
        assert!(store.message(&sent_message.id).await.unwrap().is_none());
        assert_eq!(
            store.message(&sent_new_id).await.unwrap().unwrap().mailbox,
            sent_target
        );
        assert!(store.message(&literal_message.id).await.unwrap().is_none());
        assert!(
            store.message(&dotted_message.id).await.unwrap().is_none(),
            "dotted legacy row: {:?}",
            store.message(&dotted_message.id).await.unwrap()
        );
        assert_eq!(
            store
                .message(&stable_message_id(account.id, &literal_target, 8))
                .await
                .unwrap()
                .unwrap()
                .mailbox,
            literal_target,
            "a literal Sent:: name must not be reclassified as a Sent alias"
        );
        assert_eq!(
            store
                .message(&stable_message_id(account.id, &dotted_target, 9))
                .await
                .unwrap()
                .unwrap()
                .mailbox,
            dotted_target,
            "raw spaces, Unicode, dot hierarchy and literal slash stay distinct"
        );
        let cached = store
            .cached_message_content(&sent_new_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.attachments[0].message_id, sent_new_id);
        assert_eq!(cached.attachments[0].id, format!("{sent_new_id}:mime-v1:1"));
        assert_eq!(
            store
                .cached_search_body_text(&sent_new_id)
                .await
                .unwrap()
                .as_deref(),
            Some("provider body")
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT id FROM attachments")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            format!("{sent_new_id}:mime-v1:1")
        );
        assert_eq!(
            store
                .list_message_mailbox_memberships(account.id, &sent_new_id)
                .await
                .unwrap()
                .len(),
            1
        );
        for table in [
            "mailbox_catalog_state",
            "mailbox_sync_state",
            "mailbox_action_tombstones",
            "contacted_people_backfill_sources",
        ] {
            let moved: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ? AND mailbox = ?"
            ))
            .bind(&account_key)
            .bind(&sent_target)
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(moved, 1, "{table}");
        }
        let cursor: (String, String, bool) = sqlx::query_as("SELECT last_account_id, last_mailbox, complete FROM opaque_mailbox_storage_identity_progress WHERE singleton = 1")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(
            !cursor.2,
            "the first bounded batch leaves continuation work"
        );
    }

    #[tokio::test]
    async fn opaque_mailbox_locator_migration_is_idempotent_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opaque-locator.db");
        let store = Store::open(&database).await.unwrap();
        let account = AccountDraft {
            email: "opaque-restart@example.test".into(),
            display_name: "Opaque restart".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let old_mailbox = "Sent::Archive Copy";
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Archive Copy".into(),
                    local_path: Some(old_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(44),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let mut row = message("restart", "body");
        row.id = stable_message_id(account.id, old_mailbox, 44);
        row.account_id = account.id.to_string();
        row.thread_id = row.id.clone();
        row.mailbox = old_mailbox.into();
        row.uid = 44;
        store.upsert_messages(&[row]).await.unwrap();
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, 'Archive Copy', 44, 1, 1, ?)")
            .bind(account.id.to_string())
            .bind(old_mailbox)
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();
        drop(store);

        let restarted = Store::open(&database).await.unwrap();
        let target = special_mailbox_storage_identity("Sent", "Archive Copy");
        let new_id = stable_message_id(account.id, &target, 44);
        assert!(restarted.message(&new_id).await.unwrap().is_some());
        drop(restarted);
        let restarted_again = Store::open(&database).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&restarted_again.pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "a completed migration is safe to resume");
        assert!(restarted_again.message(&new_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn opaque_locator_migration_restarts_a_large_single_mailbox_without_losing_identity_proof(
    ) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opaque-single-mailbox-batch.db");
        let store = Store::open(&database).await.unwrap();
        let account = AccountDraft {
            email: "opaque-single-batch@example.test".into(),
            display_name: "Opaque single batch".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let account_key = account.id.to_string();
        let legacy_mailbox = "Sent::Bulk";
        let remote_path = "Bulk";
        let uid_validity = 88_i64;
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: remote_path.into(),
                    local_path: Some(legacy_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(uid_validity),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let mut rows = Vec::new();
        for uid in
            1..=u32::try_from(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE + 1).unwrap()
        {
            let mut row = message("single mailbox batch", "body");
            row.id = stable_message_id(account.id, legacy_mailbox, uid);
            row.account_id = account_key.clone();
            row.thread_id = row.id.clone();
            row.mailbox = legacy_mailbox.into();
            row.uid = i64::from(uid);
            rows.push(row);
        }
        let last_old_id = rows.last().unwrap().id.clone();
        store.upsert_messages(&rows).await.unwrap();
        store
            .cache_search_body_text(&last_old_id, "last-row cached body")
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?)")
            .bind(&account_key)
            .bind(legacy_mailbox)
            .bind(remote_path)
            .bind(uid_validity)
            .bind(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE + 1)
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid, uid_validity) VALUES (?, ?, ?, ?, ?)")
            .bind(&account_key)
            .bind(legacy_mailbox)
            .bind(Utc::now())
            .bind(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE + 1)
            .bind(uid_validity)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)",
        )
        .bind(&account_key)
        .bind(&last_old_id)
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();
        drop(store);

        // Simulate interruption directly after the first durable batch.
        let first = Store::open(&database).await.unwrap();
        let target = special_mailbox_storage_identity("Sent", remote_path);
        let legacy_remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_key)
        .bind(legacy_mailbox)
        .fetch_one(&first.pool)
        .await
        .unwrap();
        assert_eq!(legacy_remaining, 1);
        for table in ["mailbox_catalog_state", "mailbox_sync_state"] {
            let source_count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ? AND mailbox = ?"
            ))
            .bind(&account_key)
            .bind(legacy_mailbox)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            let target_count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ? AND mailbox = ?"
            ))
            .bind(&account_key)
            .bind(&target)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            assert_eq!(source_count, 1, "{table} retains restart proof");
            assert_eq!(target_count, 1, "{table} is available to new work");
        }
        drop(first);

        // A new process can continue the same source mailbox. It is not
        // rejected because the source UIDVALIDITY record remains until this
        // final message has committed.
        let resumed = Store::open(&database).await.unwrap();
        let new_last_id = stable_message_id(
            account.id,
            &target,
            u32::try_from(OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE + 1).unwrap(),
        );
        let legacy_remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_key)
        .bind(legacy_mailbox)
        .fetch_one(&resumed.pool)
        .await
        .unwrap();
        assert_eq!(legacy_remaining, 0);
        let target_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_key)
        .bind(&target)
        .fetch_one(&resumed.pool)
        .await
        .unwrap();
        assert_eq!(
            target_count,
            OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE + 1
        );
        for table in ["mailbox_catalog_state", "mailbox_sync_state"] {
            let source_count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ? AND mailbox = ?"
            ))
            .bind(&account_key)
            .bind(legacy_mailbox)
            .fetch_one(&resumed.pool)
            .await
            .unwrap();
            assert_eq!(source_count, 0, "{table} is finalized only at exhaustion");
        }
        assert_eq!(
            resumed
                .cached_search_body_text(&new_last_id)
                .await
                .unwrap()
                .as_deref(),
            Some("last-row cached body")
        );
        let backfill_marker: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_backfill_messages WHERE account_id = ? AND message_id = ?",
        )
        .bind(&account_key)
        .bind(&new_last_id)
        .fetch_one(&resumed.pool)
        .await
        .unwrap();
        assert_eq!(backfill_marker, 1);

        // Opaque target rows are skipped on a later keyset pass, then the
        // durable cursor reaches completion without revisiting old rows.
        for _ in 0..3 {
            resumed
                .migrate_opaque_mailbox_storage_identities()
                .await
                .unwrap();
        }
        let complete: bool = sqlx::query_scalar(
            "SELECT complete FROM opaque_mailbox_storage_identity_progress WHERE singleton = 1",
        )
        .fetch_one(&resumed.pool)
        .await
        .unwrap();
        assert!(complete);
    }

    #[tokio::test]
    async fn opaque_locator_migration_keeps_unflagged_standard_inbox_legacy() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "legacy-inbox@example.test".into(),
            display_name: "Legacy inbox".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let mut inbox = message("legacy inbox", "body");
        inbox.id = stable_message_id(account.id, "INBOX", 1);
        inbox.account_id = account.id.to_string();
        inbox.thread_id = inbox.id.clone();
        inbox.mailbox = "INBOX".into();
        store.upsert_messages(&[inbox.clone()]).await.unwrap();
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, 'INBOX', 'INBOX', 9, 1, 1, ?)")
            .bind(account.id.to_string())
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        // This is exactly the pre-discovery profile shape. The seed has no
        // special-use flag, so preserving INBOX must not depend on one.
        store.migrate_selectable_mailboxes().await.unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();
        assert_eq!(
            store.message(&inbox.id).await.unwrap().unwrap().mailbox,
            "INBOX"
        );
    }

    #[tokio::test]
    async fn opaque_locator_migration_defers_ambiguous_rows_until_catalogue_discovery() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "ambiguous-locator@example.test".into(),
            display_name: "Ambiguous locator".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let old_mailbox = "Sent::Foo";
        let mut row = message("ambiguous", "body");
        row.id = stable_message_id(account.id, old_mailbox, 3);
        row.account_id = account.id.to_string();
        row.thread_id = row.id.clone();
        row.mailbox = old_mailbox.into();
        row.uid = 3;
        store.upsert_messages(&[row.clone()]).await.unwrap();
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, 'Foo', 4, 1, 1, ?)")
            .bind(account.id.to_string())
            .bind(old_mailbox)
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();
        assert!(store.message(&row.id).await.unwrap().is_some());
        let unresolved: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM opaque_mailbox_storage_identity_unresolved")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(unresolved, 1);
        // The cursor reaches the end on the next maintenance turn rather
        // than repeatedly retrying the same unproven row.
        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();
        let complete: bool = sqlx::query_scalar(
            "SELECT complete FROM opaque_mailbox_storage_identity_progress WHERE singleton = 1",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(complete);
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Foo".into(),
                    local_path: Some(old_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(4),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();
        let target = special_mailbox_storage_identity("Sent", "Foo");
        assert!(store
            .message(&stable_message_id(account.id, &target, 3))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn opaque_locator_backfill_stays_bounded_and_keeps_background_maintenance_alive() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "opaque-batch@example.test".into(),
            display_name: "Opaque batch".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let mut rows = Vec::new();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..=OPAQUE_MAILBOX_STORAGE_IDENTITIES_MIGRATION_BATCH_SIZE {
            let mailbox = format!("Projects-{index:04}");
            sqlx::query("INSERT INTO selectable_mailboxes(id, account_id, remote_path, local_path, hierarchy_delimiter, parent_id, parent_path, special_use, selectable, uid_validity, catalogue_coverage, updated_at) VALUES (?, ?, ?, ?, NULL, NULL, NULL, NULL, 1, 9, 'complete', ?)")
                .bind(format!("opaque-batch-mailbox-{index}"))
                .bind(account.id.to_string())
                .bind(&mailbox)
                .bind(&mailbox)
                .bind(Utc::now())
                .execute(&mut *tx)
                .await
                .unwrap();
            let mut row = message("batch", "body");
            row.id = stable_message_id(account.id, &mailbox, 1);
            row.account_id = account.id.to_string();
            row.thread_id = row.id.clone();
            row.mailbox = mailbox;
            rows.push(row);
        }
        tx.commit().await.unwrap();
        store.upsert_messages(&rows).await.unwrap();
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();

        let first = store.advance_search_catalogue_v2_backfill().await.unwrap();
        assert!(
            !first.complete,
            "the UI worker must continue after 500 moves"
        );
        let legacy_after_first: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE mailbox NOT LIKE 'Mailbox::@dakia-mailbox-v1:%'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(legacy_after_first, 1);
        assert!(
            !store
                .advance_search_catalogue_v2_backfill()
                .await
                .unwrap()
                .complete
        );
        assert!(
            store
                .advance_search_catalogue_v2_backfill()
                .await
                .unwrap()
                .complete
        );
        let legacy_after_complete: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE mailbox NOT LIKE 'Mailbox::@dakia-mailbox-v1:%'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(legacy_after_complete, 0);
    }

    #[tokio::test]
    async fn opaque_locator_migration_merges_a_verified_provider_published_duplicate() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "opaque-collision@example.test".into(),
            display_name: "Opaque collision".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let old_mailbox = "Sent::Foo";
        let target = special_mailbox_storage_identity("Sent", "Foo");
        store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Foo".into(),
                    local_path: Some(old_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(77),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let mut old = message("old", "body");
        old.id = stable_message_id(account.id, old_mailbox, 5);
        old.account_id = account.id.to_string();
        old.thread_id = old.id.clone();
        old.mailbox = old_mailbox.into();
        old.uid = 5;
        let mut published = old.clone();
        published.id = stable_message_id(account.id, &target, 5);
        published.thread_id = published.id.clone();
        published.mailbox = target.clone();
        published.is_read = true;
        store
            .upsert_messages(&[old.clone(), published.clone()])
            .await
            .unwrap();
        store
            .cache_search_body_text(&old.id, "preserved cached body")
            .await
            .unwrap();
        for mailbox in [old_mailbox, target.as_str()] {
            sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, 'Foo', 77, 1, 1, ?)")
                .bind(account.id.to_string())
                .bind(mailbox)
                .bind(Utc::now())
                .execute(&store.pool)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM opaque_mailbox_storage_identity_progress")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .migrate_opaque_mailbox_storage_identities()
            .await
            .unwrap();
        assert!(store.message(&old.id).await.unwrap().is_none());
        assert!(store.message(&published.id).await.unwrap().unwrap().is_read);
        assert_eq!(
            store
                .cached_search_body_text(&published.id)
                .await
                .unwrap()
                .as_deref(),
            Some("preserved cached body")
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn mail_rebuild_job_survives_until_explicitly_completed() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "rebuild-job@dakia.dev".into(),
            display_name: "Rebuild job".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let account_id = account.id;
        let job = MailRebuildJob {
            account_id,
            phase: "downloading".into(),
            completed: 150,
            total: Some(1_200),
            reset_before_sync: true,
        };

        store.save_mail_rebuild_job(&job).await.unwrap();
        let restored = store.mail_rebuild_jobs().await.unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].account_id, account_id);
        assert_eq!(restored[0].phase, "downloading");
        assert_eq!(restored[0].completed, 150);
        assert_eq!(restored[0].total, Some(1_200));
        assert!(restored[0].reset_before_sync);

        store.delete_mail_rebuild_job(account_id).await.unwrap();
        assert!(store.mail_rebuild_jobs().await.unwrap().is_empty());
    }
    use crate::{account::AccountDraft, provider};

    fn message(subject: &str, body: &str) -> MailSummary {
        MailSummary {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: uuid::Uuid::new_v4().to_string(),
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: uuid::Uuid::new_v4().to_string(),
            subject: subject.into(),
            from_name: Some("Mara Vaher".into()),
            from_address: "mara@example.com".into(),
            to_addresses: "you@example.com".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: body.into(),
            body_text: body.into(),
            body_html: None,
            content_state: "complete".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: false,
            is_flagged: false,
            is_answered: false,
            is_draft: false,
            has_attachments: false,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn classification_policy_upgrade_requeues_only_model_owned_categories_once() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("classification-policy.db");
        let store = Store::open(&database).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "classification-policy@example.test");
        store.save_account(&account).await.unwrap();

        let mut model_owned = message("Dispatched: an order", "");
        model_owned.id = "model-owned".into();
        model_owned.account_id = account_id.to_string();
        model_owned.category = Some("people".into());
        model_owned.classification_confidence = Some(0.91);
        model_owned.classification_source = Some("model".into());
        let mut user_owned = message("A category chosen by the user", "");
        user_owned.id = "user-owned".into();
        user_owned.account_id = account_id.to_string();
        user_owned.uid = 2;
        user_owned.category = Some("people".into());
        user_owned.classification_confidence = Some(1.0);
        user_owned.classification_source = Some("user".into());
        store
            .upsert_messages(&[model_owned, user_owned])
            .await
            .unwrap();
        sqlx::query("UPDATE app_meta SET value = '1' WHERE key = 'classification_policy_version'")
            .execute(&store.pool)
            .await
            .unwrap();
        drop(store);

        let migrated = Store::open(&database).await.unwrap();
        migrated
            .claim_classification_revision("policy-owner", "test-model")
            .await
            .unwrap();
        let pending = migrated.message("model-owned").await.unwrap().unwrap();
        assert_eq!(pending.category.as_deref(), Some("people"));
        assert_eq!(pending.classification_source, None);
        assert_eq!(pending.classification_confidence, None);
        let preserved = migrated.message("user-owned").await.unwrap().unwrap();
        assert_eq!(preserved.category.as_deref(), Some("people"));
        assert_eq!(preserved.classification_source.as_deref(), Some("user"));
        assert_eq!(preserved.classification_confidence, Some(1.0));

        let update = ModelClassificationUpdate::from_message(
            &pending,
            "transactions".into(),
            Some(1.0),
            false,
            "policy-owner",
            "test-model",
        );
        migrated
            .apply_model_classifications(&[update])
            .await
            .unwrap();
        drop(migrated);

        let reopened = Store::open(&database).await.unwrap();
        let classified = reopened.message("model-owned").await.unwrap().unwrap();
        assert_eq!(classified.category.as_deref(), Some("transactions"));
        assert_eq!(classified.classification_source.as_deref(), Some("model"));
        assert_eq!(classified.classification_confidence, Some(1.0));

        let old_policy_update = ModelClassificationUpdate::from_message(
            &classified,
            "people".into(),
            Some(0.99),
            false,
            "policy-owner",
            "test-model",
        );
        sqlx::query("UPDATE app_meta SET value = 'future-policy' WHERE key = 'classification_policy_version'")
            .execute(&reopened.pool)
            .await
            .unwrap();
        reopened
            .apply_model_classifications(&[old_policy_update])
            .await
            .unwrap();
        assert_eq!(
            reopened
                .message("model-owned")
                .await
                .unwrap()
                .unwrap()
                .category
                .as_deref(),
            Some("transactions"),
            "an in-flight result from an older policy must fail the final CAS"
        );
    }

    #[tokio::test]
    async fn classification_model_revision_requeues_model_owned_rows_only_on_change() {
        let store = Store::in_memory().await.unwrap();
        let mut model_owned = message("Model-owned decision", "original evidence");
        model_owned.id = "revision-model".into();
        model_owned.category = Some("people".into());
        model_owned.classification_confidence = Some(0.9);
        model_owned.classification_source = Some("model".into());
        let mut user_owned = message("User-owned decision", "original evidence");
        user_owned.id = "revision-user".into();
        user_owned.uid = 2;
        user_owned.category = Some("people".into());
        user_owned.classification_confidence = Some(1.0);
        user_owned.classification_source = Some("user".into());
        store
            .upsert_messages(&[model_owned, user_owned])
            .await
            .unwrap();

        store
            .claim_classification_revision("revision-owner-a", "model-a")
            .await
            .unwrap();
        assert_eq!(
            store
                .message("revision-model")
                .await
                .unwrap()
                .unwrap()
                .classification_source,
            None
        );
        assert_eq!(
            store
                .message("revision-user")
                .await
                .unwrap()
                .unwrap()
                .classification_source
                .as_deref(),
            Some("user")
        );

        let pending = store.message("revision-model").await.unwrap().unwrap();
        store
            .apply_model_classifications(&[ModelClassificationUpdate::from_message(
                &pending,
                "notifications".into(),
                Some(0.8),
                false,
                "revision-owner-a",
                "model-a",
            )])
            .await
            .unwrap();
        store
            .claim_classification_revision("revision-owner-a", "model-a")
            .await
            .unwrap();
        assert_eq!(
            store
                .message("revision-model")
                .await
                .unwrap()
                .unwrap()
                .classification_source
                .as_deref(),
            Some("model")
        );

        let old_model_update = ModelClassificationUpdate::from_message(
            &store.message("revision-model").await.unwrap().unwrap(),
            "people".into(),
            Some(0.99),
            false,
            "revision-owner-a",
            "model-a",
        );
        assert_eq!(
            store
                .claim_classification_revision("revision-owner-b", "model-b")
                .await
                .unwrap_err()
                .to_string(),
            "another email classifier is active; retry after it finishes"
        );
        store
            .release_classification_revision("revision-owner-a")
            .await
            .unwrap();
        store
            .claim_classification_revision("revision-owner-b", "model-b")
            .await
            .unwrap();
        store
            .apply_model_classifications(&[old_model_update])
            .await
            .unwrap();
        assert_eq!(
            store
                .message("revision-model")
                .await
                .unwrap()
                .unwrap()
                .classification_source,
            None
        );
    }

    #[tokio::test]
    async fn concurrent_stores_cannot_ping_pong_active_classifier_revisions() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("classifier-revision-lease.db");
        let first = Store::open(&database).await.unwrap();
        let second = Store::open(&database).await.unwrap();

        first
            .claim_classification_revision("owner-a", "model-a")
            .await
            .unwrap();
        assert!(second
            .claim_classification_revision("owner-b", "model-b")
            .await
            .unwrap_err()
            .to_string()
            .contains("another email classifier is active"));
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM app_meta WHERE key = 'classification_model_revision'",
            )
            .fetch_one(&second.pool)
            .await
            .unwrap(),
            "model-a"
        );

        first
            .release_classification_revision("owner-a")
            .await
            .unwrap();
        second
            .claim_classification_revision("owner-b", "model-b")
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM app_meta WHERE key = 'classification_model_revision'",
            )
            .fetch_one(&first.pool)
            .await
            .unwrap(),
            "model-b"
        );

        sqlx::query(
            "UPDATE app_meta SET value = ? WHERE key = 'classification_revision_active_at'",
        )
        .bind((Utc::now().timestamp() - CLASSIFICATION_REVISION_ACTIVITY_SECONDS - 1).to_string())
        .execute(&second.pool)
        .await
        .unwrap();
        first
            .claim_classification_revision("owner-c", "model-c")
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM app_meta WHERE key = 'classification_model_revision'",
            )
            .fetch_one(&second.pool)
            .await
            .unwrap(),
            "model-c",
            "an expired owner must not block crash recovery"
        );
    }

    #[tokio::test]
    async fn classification_signal_changes_requeue_only_model_owned_categories() {
        let store = Store::in_memory().await.unwrap();
        store
            .claim_classification_revision("test-owner", "test-model")
            .await
            .unwrap();
        let mut model_owned = message("Account update", "");
        model_owned.id = "signal-model".into();
        model_owned.category = Some("people".into());
        model_owned.classification_confidence = Some(0.8);
        model_owned.classification_source = Some("model".into());
        let mut user_owned = message("User override", "");
        user_owned.id = "signal-user".into();
        user_owned.category = Some("people".into());
        user_owned.classification_confidence = Some(1.0);
        user_owned.classification_source = Some("user".into());
        store
            .upsert_messages(&[model_owned, user_owned])
            .await
            .unwrap();

        let stale_update = ModelClassificationUpdate::from_message(
            &store.message("signal-model").await.unwrap().unwrap(),
            "people".into(),
            Some(0.8),
            false,
            "test-owner",
            "test-model",
        );

        store
            .update_classification_signals(&[
                (
                    "signal-model".into(),
                    "Mailing-list unsubscribe header present".into(),
                ),
                (
                    "signal-user".into(),
                    "Mailing-list unsubscribe header present".into(),
                ),
            ])
            .await
            .unwrap();
        store
            .apply_model_classifications(&[stale_update])
            .await
            .unwrap();

        let pending = store.message("signal-model").await.unwrap().unwrap();
        assert_eq!(pending.category.as_deref(), Some("people"));
        assert_eq!(pending.classification_source, None);
        assert_eq!(pending.classification_confidence, None);
        let preserved = store.message("signal-user").await.unwrap().unwrap();
        assert_eq!(preserved.category.as_deref(), Some("people"));
        assert_eq!(preserved.classification_source.as_deref(), Some("user"));
        assert_eq!(preserved.classification_confidence, Some(1.0));
    }

    #[tokio::test]
    async fn changed_message_evidence_requeues_and_rejects_an_in_flight_model_result() {
        let store = Store::in_memory().await.unwrap();
        store
            .claim_classification_revision("test-owner", "test-model")
            .await
            .unwrap();
        let mut original = message("Original subject", "original preview");
        original.id = "changing-evidence".into();
        original.category = Some("people".into());
        original.classification_confidence = Some(0.9);
        original.classification_source = Some("model".into());
        store
            .upsert_messages(std::slice::from_ref(&original))
            .await
            .unwrap();
        let stale_update = ModelClassificationUpdate::from_message(
            &store.message("changing-evidence").await.unwrap().unwrap(),
            "people".into(),
            Some(0.9),
            false,
            "test-owner",
            "test-model",
        );

        original.subject = "Dispatched: replacement order".into();
        original.snippet = "Your replacement has shipped.".into();
        store.upsert_messages(&[original]).await.unwrap();
        store
            .apply_model_classifications(&[stale_update])
            .await
            .unwrap();

        let changed = store.message("changing-evidence").await.unwrap().unwrap();
        assert_eq!(changed.subject, "Dispatched: replacement order");
        assert_eq!(changed.classification_source, None);
        assert_eq!(changed.classification_confidence, None);
    }

    #[tokio::test]
    async fn known_correspondent_evidence_is_account_scoped_and_rfc_parsed() {
        let store = Store::in_memory().await.unwrap();
        store
            .claim_classification_revision("test-owner", "test-model")
            .await
            .unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let other_account_id = uuid::Uuid::new_v4().to_string();
        let mut incoming = message("Re: Project", "A real follow-up");
        incoming.id = "known-correspondence".into();
        incoming.account_id = account_id.clone();
        incoming.thread_id = "shared-thread-name".into();
        incoming.category = Some("other".into());
        incoming.classification_source = Some("model".into());
        incoming.classification_confidence = Some(0.7);
        let mut sent = message("Re: Project", "My prior reply");
        sent.id = "sent-member".into();
        sent.account_id = account_id;
        sent.thread_id = "older-independent-thread".into();
        sent.mailbox = "Sent::Sent Messages".into();
        sent.uid = 2;
        sent.to_addresses = incoming.from_address.clone();
        let mut wrong_sender = message("Re: Project", "A forged thread member");
        wrong_sender.id = "wrong-sender".into();
        wrong_sender.account_id = sent.account_id.clone();
        wrong_sender.thread_id = incoming.thread_id.clone();
        wrong_sender.from_address = "bulk-sender@example.test".into();
        wrong_sender.uid = 3;
        wrong_sender.category = Some("other".into());
        wrong_sender.classification_source = Some("model".into());
        wrong_sender.classification_confidence = Some(0.7);
        let mut display_name_spoof = message("Unrelated sender", "Not a correspondent");
        display_name_spoof.id = "display-name-spoof".into();
        display_name_spoof.account_id = sent.account_id.clone();
        display_name_spoof.from_address = "victim@example.com".into();
        display_name_spoof.uid = 4;
        let mut sent_with_address_in_display_name = message("Prior sent", "A different recipient");
        sent_with_address_in_display_name.id = "sent-display-name".into();
        sent_with_address_in_display_name.account_id = sent.account_id.clone();
        sent_with_address_in_display_name.mailbox = "Sent::Sent Messages".into();
        sent_with_address_in_display_name.uid = 4;
        sent_with_address_in_display_name.to_addresses =
            "\"victim@example.com\" <actual@example.net>".into();
        let mut cross_account = message("Re: Unrelated", "Not my correspondent");
        cross_account.id = "cross-account".into();
        cross_account.account_id = other_account_id;
        cross_account.thread_id = incoming.thread_id.clone();
        let stale_update = ModelClassificationUpdate::from_message(
            &incoming,
            "other".into(),
            Some(0.7),
            false,
            "test-owner",
            "test-model",
        );
        store
            .upsert_messages(&[
                incoming.clone(),
                sent.clone(),
                wrong_sender.clone(),
                display_name_spoof.clone(),
                sent_with_address_in_display_name,
                cross_account.clone(),
            ])
            .await
            .unwrap();

        let known = store
            .messages_from_known_correspondents(&[
                incoming,
                wrong_sender,
                display_name_spoof,
                cross_account,
            ])
            .await
            .unwrap();
        assert_eq!(known, HashSet::from(["known-correspondence".into()]));
        store
            .apply_model_classifications(&[stale_update])
            .await
            .unwrap();
        assert_eq!(
            store
                .message("known-correspondence")
                .await
                .unwrap()
                .unwrap()
                .classification_source,
            None
        );
        assert_eq!(
            store
                .message("wrong-sender")
                .await
                .unwrap()
                .unwrap()
                .classification_source
                .as_deref(),
            Some("model")
        );

        let current = store
            .message("known-correspondence")
            .await
            .unwrap()
            .unwrap();
        store
            .apply_model_classifications(&[ModelClassificationUpdate::from_message(
                &current,
                "people".into(),
                None,
                true,
                "test-owner",
                "test-model",
            )])
            .await
            .unwrap();
        store.upsert_messages(&[sent]).await.unwrap();
        assert_eq!(
            store
                .message("known-correspondence")
                .await
                .unwrap()
                .unwrap()
                .classification_source
                .as_deref(),
            Some("model"),
            "refreshing an already-indexed Sent message must not requeue history"
        );
    }

    fn cached_content(body_text: &str) -> CachedMessageContent {
        CachedMessageContent {
            body_text: body_text.into(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: Vec::new(),
        }
    }

    fn cache_candidate_message(
        account_id: uuid::Uuid,
        id: &str,
        uid: i64,
        mailbox: &str,
        received_at: DateTime<Utc>,
    ) -> MailSummary {
        let mut message = message(id, "preview");
        message.id = id.into();
        message.account_id = account_id.to_string();
        message.uid = uid;
        message.mailbox = mailbox.into();
        message.received_at = received_at;
        message
    }

    fn account_with_id(id: uuid::Uuid, email: &str) -> Account {
        let mut account = AccountDraft {
            email: email.into(),
            display_name: "Storage test".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        account.id = id;
        account
    }

    async fn save_test_account(store: &Store, id: uuid::Uuid) {
        store
            .save_account(&account_with_id(id, &format!("{id}@example.test")))
            .await
            .unwrap();
    }

    fn contacted_recipient(address: &str, display_name: Option<&str>) -> ContactedPersonRecipient {
        ContactedPersonRecipient {
            address: address.into(),
            display_name: display_name.map(str::to_owned),
            formatted_address: None,
        }
    }

    #[tokio::test]
    async fn contacted_people_legacy_migrations_are_bounded_restart_safe_and_stable_at_fifty_thousand_rows(
    ) {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let account_id = account_id.to_string();

        // Seed the shape left by builds from before normalized autocomplete
        // fields and provider-safe Sent source identities. A temporary sequence
        // keeps this realistic 50,000-row fixture to a handful of bulk SQL
        // statements instead of 150,000 individual test queries.
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE contacted_people_migration_seed(n INTEGER PRIMARY KEY)")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "WITH RECURSIVE seed(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM seed WHERE n < 50000) INSERT INTO contacted_people_migration_seed(n) SELECT n FROM seed",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) SELECT printf('person%05d@example.test', n), CASE WHEN n = 25001 THEN 'José Legacy' ELSE printf('Legacy Person %d', n) END, CASE WHEN n = 25001 THEN 'José Legacy <person25001@example.test>' ELSE printf('Legacy Person %d <person%05d@example.test>', n, n) END, '2024-01-01T00:00:00Z', '2026-01-01T00:00:00Z', (n % 7) + 1, NULL FROM contacted_people_migration_seed",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count, display_name, formatted_address) SELECT printf('person%05d@example.test', n), ?, '2024-01-01T00:00:00Z', '2026-01-01T00:00:00Z', (n % 7) + 1, CASE WHEN n = 25001 THEN 'José Legacy' ELSE printf('Legacy Person %d', n) END, CASE WHEN n = 25001 THEN 'José Legacy <person25001@example.test>' ELSE printf('Legacy Person %d <person%05d@example.test>', n, n) END FROM contacted_people_migration_seed",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages(id, account_id, mailbox, uid, message_id, thread_id, subject, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text) SELECT printf('legacy-message-%05d', n), ?, 'Sent', n, printf('<legacy-%05d@example.test>', n), printf('legacy-thread-%05d', n), printf('Legacy sent message %d', n), 'owner@example.test', printf('person%05d@example.test', n), '', '', '', '2026-01-01T00:00:00Z', '', '' FROM contacted_people_migration_seed",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, 'Sent', 'Sent', 4242, 50000, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO contacted_people_backfill_messages(account_id, message_id) SELECT ?, printf('legacy-message-%05d', n) FROM contacted_people_migration_seed",
        )
        .bind(&account_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        for key in [
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_CURSOR_KEY,
            CONTACTED_PEOPLE_NORMALIZED_MIGRATION_COMPLETE_KEY,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY,
            CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY,
        ] {
            sqlx::query("DELETE FROM app_meta WHERE key = ?")
                .bind(key)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let stable_person_before: (i64, String, String, i64, String, String) = sqlx::query_as(
            "SELECT person.rowid, person.canonical_address, person.formatted_address, stats.send_count, stats.first_contacted_at, stats.last_contacted_at FROM contacted_people person JOIN contacted_people_account_stats stats ON stats.canonical_address = person.canonical_address WHERE person.canonical_address = 'person25001@example.test' AND stats.account_id = ?",
        )
        .bind(&account_id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        store.pool.close().await;
        drop(store);

        let store = Store::open(&database).await.unwrap();
        let first_normalized: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people WHERE normalized_address <> ''",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let first_sources: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(first_normalized, CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE);
        assert_eq!(first_sources, CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE);
        let first_cursors: (String, String) = sqlx::query_as(
            "SELECT (SELECT value FROM app_meta WHERE key = ?), (SELECT value FROM app_meta WHERE key = ?)",
        )
        .bind(CONTACTED_PEOPLE_NORMALIZED_MIGRATION_CURSOR_KEY)
        .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            first_cursors.0,
            CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE.to_string()
        );
        assert_eq!(
            first_cursors.1,
            CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE.to_string()
        );
        store.pool.close().await;
        drop(store);

        let store = Store::open(&database).await.unwrap();
        let second_normalized: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people WHERE normalized_address <> ''",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let second_sources: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            second_normalized,
            CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE * 2,
            "reopen must resume from the durable normalized-person cursor"
        );
        assert_eq!(
            second_sources,
            CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE * 2,
            "reopen must resume from the durable legacy-source cursor"
        );

        let mut progress = ContactedPeopleMigrationProgress {
            normalized_people: 0,
            source_markers: 0,
            changed_people: 0,
            complete: false,
        };
        while !progress.complete {
            progress = store
                .continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await
                .unwrap();
            assert!(progress.normalized_people <= CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as usize);
            assert!(progress.source_markers <= CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as usize);
        }

        let final_counts: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM contacted_people WHERE normalized_address <> ''), (SELECT COUNT(*) FROM contacted_people_backfill_sources), (SELECT COUNT(*) FROM contacted_people_backfill_rfc_messages), (SELECT COUNT(*) FROM contacted_people_backfill_messages)",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(final_counts, (50_000, 50_000, 50_000, 50_000));
        let stable_person_after: (i64, String, String, i64, String, String) = sqlx::query_as(
            "SELECT person.rowid, person.canonical_address, person.formatted_address, stats.send_count, stats.first_contacted_at, stats.last_contacted_at FROM contacted_people person JOIN contacted_people_account_stats stats ON stats.canonical_address = person.canonical_address WHERE person.canonical_address = 'person25001@example.test' AND stats.account_id = ?",
        )
        .bind(&account_id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(stable_person_after, stable_person_before);
        let migrated_name: (String, String) = sqlx::query_as(
            "SELECT normalized_display_name, normalized_display_tokens FROM contacted_people WHERE canonical_address = 'person25001@example.test'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            migrated_name.0,
            normalize_contacted_people_match("José Legacy")
        );
        assert_eq!(
            migrated_name.1,
            normalize_contacted_people_tokens("José Legacy")
        );

        store.pool.close().await;
        drop(store);
        let store = Store::open(&database).await.unwrap();
        let completed = store
            .continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
            .await
            .unwrap();
        assert!(completed.complete);
        assert_eq!(completed.normalized_people, 0);
        assert_eq!(completed.source_markers, 0);
    }

    #[tokio::test]
    async fn enabled_contacted_people_aggregate_migration_is_bounded_restart_safe_and_skips_unchanged_saves(
    ) {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("enabled-contacted-people-aggregates.db");
        let store = Store::open(&database).await.unwrap();
        let disabled = uuid::Uuid::new_v4();
        let active = uuid::Uuid::new_v4();
        save_test_account(&store, disabled).await;
        save_test_account(&store, active).await;
        let disabled_key = disabled.to_string();
        let active_key = active.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..50_000_i64 {
            let address = format!("aggregate{index:05}@example.test");
            sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES (?, 'Alice Disabled', ?, '2026-01-01T00:00:00Z', '2026-02-01T00:00:00Z', 6, NULL)")
                .bind(&address)
                .bind(format!("Alice Disabled <{address}>"))
                .execute(&mut *tx)
                .await
                .unwrap();
            for (account_id, display_name, count) in [
                (&disabled_key, "Alice Disabled", 5_i64),
                (&active_key, "Bob Active", 1_i64),
            ] {
                sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count, display_name, formatted_address) VALUES (?, ?, '2026-01-01T00:00:00Z', '2026-02-01T00:00:00Z', ?, ?, ?)")
                    .bind(&address)
                    .bind(account_id)
                    .bind(count)
                    .bind(display_name)
                    .bind(format!("{display_name} <{address}>"))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
        }
        tx.commit().await.unwrap();

        let mut disabled_account = store.account(disabled).await.unwrap().unwrap();
        disabled_account.enabled = false;
        store.save_account(&disabled_account).await.unwrap();
        let cursor: Option<String> = sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY)
            .fetch_optional(&store.pool)
            .await
            .unwrap();
        assert!(
            cursor.is_some(),
            "the first save only commits one 500-person batch"
        );
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        for _ in 0..200 {
            let complete: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM app_meta WHERE key = ? AND value = '1')",
            )
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY)
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
            if complete {
                break;
            }
            reopened
                .continue_contacted_people_migrations(CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE as u32)
                .await
                .unwrap();
        }
        let complete: Option<String> =
            sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
                .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY)
                .fetch_optional(&reopened.pool)
                .await
                .unwrap();
        assert_eq!(complete.as_deref(), Some("1"));
        let sample: (i64, String) = sqlx::query_as("SELECT send_count, display_name FROM contacted_people WHERE canonical_address = 'aggregate49999@example.test'")
            .fetch_one(&reopened.pool).await.unwrap();
        assert_eq!(sample, (1, "Bob Active".into()));

        let unchanged = reopened.account(active).await.unwrap().unwrap();
        reopened.save_account(&unchanged).await.unwrap();
        let after_unchanged: Option<String> =
            sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
                .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_VERSION_KEY)
                .fetch_optional(&reopened.pool)
                .await
                .unwrap();
        assert_eq!(after_unchanged.as_deref(), Some("1"));
        let cursor: Option<String> = sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
            .bind(CONTACTED_PEOPLE_ENABLED_AGGREGATES_CURSOR_KEY)
            .fetch_optional(&reopened.pool)
            .await
            .unwrap();
        assert!(
            cursor.is_none(),
            "an unchanged active account save is a no-op"
        );
    }

    #[tokio::test]
    async fn successful_outgoing_recipients_are_deduplicated_exclude_owners_and_restore_hidden() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let own_address = format!("{account_id}@example.test");
        let other_account = "other-account@example.test".to_owned();
        let recipients = vec![
            contacted_recipient("JANE+VIP@example.test", Some("Doe, Jane")),
            contacted_recipient("jane+vip@example.test", Some("Newer Jane")),
            contacted_recipient(&own_address, Some("Self")),
            contacted_recipient(&other_account, Some("Other self")),
            contacted_recipient("not an address", Some("Invalid")),
        ];
        assert_eq!(
            store
                .record_successful_outgoing_recipients(
                    account_id,
                    &recipients,
                    std::slice::from_ref(&other_account),
                )
                .await
                .unwrap(),
            1
        );
        let people = store
            .suggest_contacted_people("jane+", Some(account_id))
            .await
            .unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].address, "jane+vip@example.test");
        assert_eq!(people[0].send_count, 1);
        assert_eq!(people[0].display_name.as_deref(), Some("Newer Jane"));

        store
            .hide_contacted_person("JANE+VIP@example.test")
            .await
            .unwrap();
        assert!(store
            .suggest_contacted_people("jane", Some(account_id))
            .await
            .unwrap()
            .is_empty());
        store
            .record_successful_outgoing_recipients(
                account_id,
                &[contacted_recipient(
                    "jane+vip@example.test",
                    Some("Doe, Jane"),
                )],
                &[],
            )
            .await
            .unwrap();
        let restored = store
            .suggest_contacted_people("jane", Some(account_id))
            .await
            .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].send_count, 2);
        assert_eq!(
            restored[0].formatted_address,
            "\"Doe, Jane\" <jane+vip@example.test>"
        );
    }

    #[tokio::test]
    async fn contacted_people_ranking_prefers_the_selected_sending_account() {
        let store = Store::in_memory().await.unwrap();
        let first_account = uuid::Uuid::new_v4();
        let second_account = uuid::Uuid::new_v4();
        save_test_account(&store, first_account).await;
        save_test_account(&store, second_account).await;
        store
            .record_successful_outgoing_recipients(
                first_account,
                &[contacted_recipient("alex@example.test", Some("Alex"))],
                &[],
            )
            .await
            .unwrap();
        for _ in 0..3 {
            store
                .record_successful_outgoing_recipients(
                    second_account,
                    &[contacted_recipient("alice@example.test", Some("Alice"))],
                    &[],
                )
                .await
                .unwrap();
        }
        let preferred = store
            .suggest_contacted_people("al", Some(first_account))
            .await
            .unwrap();
        assert_eq!(
            preferred
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["alex@example.test", "alice@example.test"]
        );
        let global = store.suggest_contacted_people("al", None).await.unwrap();
        assert_eq!(global[0].address, "alice@example.test");
        assert_eq!(global[0].account_send_count, 0);
    }

    #[tokio::test]
    async fn contacted_people_ranking_uses_selected_account_recency_before_frequency() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        for address in ["rank-old@example.test", "rank-new@example.test"] {
            store
                .record_successful_outgoing_recipients(
                    account_id,
                    &[contacted_recipient(address, Some("Ranked"))],
                    &[],
                )
                .await
                .unwrap();
        }
        for (address, send_count, last_contacted_at) in [
            ("rank-old@example.test", 100_i64, "2026-01-01T00:00:00Z"),
            ("rank-new@example.test", 1_i64, "2026-02-01T00:00:00Z"),
        ] {
            let last_contacted_at: DateTime<Utc> = last_contacted_at.parse().unwrap();
            sqlx::query("UPDATE contacted_people_account_stats SET send_count = ?, last_contacted_at = ? WHERE canonical_address = ? AND account_id = ?")
                .bind(send_count)
                .bind(last_contacted_at)
                .bind(address)
                .bind(account_id.to_string())
                .execute(&store.pool)
                .await
                .unwrap();
            sqlx::query("UPDATE contacted_people SET send_count = ?, last_contacted_at = ? WHERE canonical_address = ?")
                .bind(send_count)
                .bind(last_contacted_at)
                .bind(address)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        for query in ["", "rank"] {
            assert_eq!(
                store
                    .suggest_contacted_people(query, Some(account_id))
                    .await
                    .unwrap()
                    .iter()
                    .map(|person| person.address.as_str())
                    .collect::<Vec<_>>(),
                vec!["rank-new@example.test", "rank-old@example.test"],
                "query {query:?}"
            );
        }
    }

    #[tokio::test]
    async fn contacted_people_empty_focus_uses_recency_before_global_frequency() {
        let store = Store::in_memory().await.unwrap();
        let first_account = uuid::Uuid::new_v4();
        let second_account = uuid::Uuid::new_v4();
        save_test_account(&store, first_account).await;
        save_test_account(&store, second_account).await;
        store
            .record_successful_outgoing_recipients(
                first_account,
                &[contacted_recipient(
                    "old-frequent@example.test",
                    Some("Old"),
                )],
                &[],
            )
            .await
            .unwrap();
        store
            .record_successful_outgoing_recipients(
                second_account,
                &[contacted_recipient("recent@example.test", Some("Recent"))],
                &[],
            )
            .await
            .unwrap();
        let old_at: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        let recent_at: DateTime<Utc> = "2026-02-01T00:00:00Z".parse().unwrap();
        sqlx::query("UPDATE contacted_people SET send_count = ?, last_contacted_at = ? WHERE canonical_address = ?")
            .bind(100_i64)
            .bind(old_at)
            .bind("old-frequent@example.test")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE contacted_people SET last_contacted_at = ? WHERE canonical_address = ?",
        )
        .bind(recent_at)
        .bind("recent@example.test")
        .execute(&store.pool)
        .await
        .unwrap();

        assert_eq!(
            store
                .suggest_contacted_people("", None)
                .await
                .unwrap()
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["recent@example.test", "old-frequent@example.test"]
        );
    }

    #[tokio::test]
    async fn contacted_people_matching_casefolds_unicode_and_ignores_common_latin_diacritics() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .record_successful_outgoing_recipients(
                account_id,
                &[contacted_recipient(
                    "josé@example.test",
                    Some("José Álvaro"),
                )],
                &[],
            )
            .await
            .unwrap();

        for query in ["JOSE", "alva", "lvar", "josé@example"] {
            assert_eq!(
                store
                    .suggest_contacted_people(query, Some(account_id))
                    .await
                    .unwrap()
                    .iter()
                    .map(|person| person.address.as_str())
                    .collect::<Vec<_>>(),
                vec!["josé@example.test"],
                "query {query:?}"
            );
        }
    }

    #[tokio::test]
    async fn contacted_people_selective_prefix_suggestions_stay_under_one_hundred_ms_at_fifty_thousand_rows(
    ) {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let timestamp: DateTime<Utc> = "2026-09-06T10:00:00Z".parse().unwrap();
        let account_key = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..50_000_i64 {
            let (address, send_count) = if index < 10 {
                (format!("needle{index}@example.test"), index + 1)
            } else {
                (format!("person{index:05}@example.test"), 1)
            };
            let formatted_address = format!("Person <{address}>");
            sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES (?, 'Person', ?, ?, ?, ?, NULL)")
                .bind(&address)
                .bind(&formatted_address)
                .bind(timestamp)
                .bind(timestamp)
                .bind(send_count)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count) VALUES (?, ?, ?, ?, ?)")
                .bind(&address)
                .bind(&account_key)
                .bind(timestamp)
                .bind(timestamp)
                .bind(send_count)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let started = Instant::now();
        let suggestions = store
            .suggest_contacted_people("needle", Some(account_id))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
        assert_eq!(suggestions.len(), 8);
        assert_eq!(
            suggestions
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec![
                "needle9@example.test",
                "needle8@example.test",
                "needle7@example.test",
                "needle6@example.test",
                "needle5@example.test",
                "needle4@example.test",
                "needle3@example.test",
                "needle2@example.test",
            ]
        );
    }

    #[tokio::test]
    async fn contacted_people_display_prefix_and_substring_suggestions_stay_under_one_hundred_ms_at_fifty_thousand_rows(
    ) {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let timestamp: DateTime<Utc> = "2026-09-06T10:00:00Z".parse().unwrap();
        let account_key = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..50_000_i64 {
            let address = format!("person{index:05}@example.test");
            let display_name = if index < 10 {
                format!("Needle Person {index}")
            } else if index < 20 {
                format!("Darlington Person {index}")
            } else {
                format!("Ordinary Person {index}")
            };
            sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at, normalized_display_name, normalized_address, normalized_display_tokens, normalized_address_tokens) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)")
                .bind(&address)
                .bind(&display_name)
                .bind(format!("{display_name} <{address}>"))
                .bind(timestamp)
                .bind(timestamp)
                .bind(index + 1)
                .bind(normalize_contacted_people_match(&display_name))
                .bind(normalize_contacted_people_match(&address))
                .bind(normalize_contacted_people_tokens(&display_name))
                .bind(normalize_contacted_people_tokens(&address))
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count) VALUES (?, ?, ?, ?, ?)")
                .bind(&address)
                .bind(&account_key)
                .bind(timestamp)
                .bind(timestamp)
                .bind(index + 1)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        for (query, expected_first) in [
            ("needle", "person00009@example.test"),
            ("arlin", "person00019@example.test"),
        ] {
            let started = Instant::now();
            let suggestions = store
                .suggest_contacted_people(query, Some(account_id))
                .await
                .unwrap();
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_millis(100),
                "{query} took {elapsed:?}"
            );
            assert_eq!(suggestions.len(), 8);
            assert_eq!(suggestions[0].address, expected_first);
        }
    }

    #[tokio::test]
    async fn contacted_people_empty_focus_suggestions_stay_under_one_hundred_ms_at_fifty_thousand_rows(
    ) {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let timestamp: DateTime<Utc> = "2026-09-06T10:00:00Z".parse().unwrap();
        let account_key = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..50_000_i64 {
            let address = format!("recent{index:05}@example.test");
            let observed_at = timestamp + chrono::Duration::seconds(index);
            sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES (?, 'Recent', ?, ?, ?, 1, NULL)")
                .bind(&address)
                .bind(format!("Recent <{address}>"))
                .bind(observed_at)
                .bind(observed_at)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count) VALUES (?, ?, ?, ?, 1)")
                .bind(&address)
                .bind(&account_key)
                .bind(observed_at)
                .bind(observed_at)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let started = Instant::now();
        let suggestions = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
        assert_eq!(suggestions.len(), 8);
        assert_eq!(
            suggestions
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec![
                "recent49999@example.test",
                "recent49998@example.test",
                "recent49997@example.test",
                "recent49996@example.test",
                "recent49995@example.test",
                "recent49994@example.test",
                "recent49993@example.test",
                "recent49992@example.test",
            ]
        );
    }

    #[tokio::test]
    async fn sent_backfill_parses_real_recipient_headers_and_marks_messages_idempotently() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut sent = message("Sent recipients", "preview");
        sent.account_id = account_id.to_string();
        sent.id = stable_message_id(account_id, "Sent", 1);
        sent.thread_id = sent.id.clone();
        sent.mailbox = "Sent".into();
        sent.uid = 1;
        sent.to_addresses = "\"Doe, Jane\" <jane+vip@example.test>, Bob <bob@example.test>".into();
        sent.cc_addresses = "Carol <carol@example.test>".into();
        sent.bcc_addresses = "Blind <blind@example.test>".into();
        sent.received_at = "2026-09-01T10:00:00Z".parse().unwrap();
        let mut empty_sent = message("No recipients", "preview");
        empty_sent.account_id = account_id.to_string();
        empty_sent.id = stable_message_id(account_id, "Sent", 2);
        empty_sent.thread_id = empty_sent.id.clone();
        empty_sent.mailbox = "Sent::Archive/Sent".into();
        empty_sent.uid = 2;
        empty_sent.to_addresses.clear();
        empty_sent.cc_addresses.clear();
        empty_sent.bcc_addresses.clear();
        empty_sent.received_at = "2026-09-02T10:00:00Z".parse().unwrap();
        let mut inbound = message("Inbound only", "preview");
        inbound.account_id = account_id.to_string();
        inbound.id = stable_message_id(account_id, "INBOX", 3);
        inbound.thread_id = inbound.id.clone();
        inbound.uid = 3;
        inbound.to_addresses = "Inbound <inbound@example.test>".into();
        store
            .upsert_messages(&[sent, empty_sent, inbound])
            .await
            .unwrap();

        let first = store
            .backfill_contacted_people_from_sent(account_id, &[], 1)
            .await
            .unwrap();
        assert_eq!(first.processed_messages, 1);
        assert!(!first.complete);
        let second = store
            .backfill_contacted_people_from_sent(account_id, &[], 1)
            .await
            .unwrap();
        assert_eq!(second.processed_messages, 1);
        assert!(second.complete);
        let people = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap();
        assert_eq!(people.len(), 4);
        let jane = people
            .iter()
            .find(|person| person.address == "jane+vip@example.test")
            .unwrap();
        assert_eq!(jane.display_name.as_deref(), Some("Doe, Jane"));
        assert_eq!(
            jane.formatted_address,
            "\"Doe, Jane\" <jane+vip@example.test>"
        );
        assert!(people
            .iter()
            .all(|person| person.address != "inbound@example.test"));
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 0,
                changed_people: 0,
                complete: true,
            }
        );
        let mut late_historical = message("Late historical Sent", "preview");
        late_historical.account_id = account_id.to_string();
        late_historical.id = stable_message_id(account_id, "Sent", 4);
        late_historical.thread_id = late_historical.id.clone();
        late_historical.mailbox = "Sent".into();
        late_historical.uid = 4;
        late_historical.to_addresses = "Late <late@example.test>".into();
        late_historical.cc_addresses.clear();
        late_historical.bcc_addresses.clear();
        late_historical.received_at = "2025-01-01T10:00:00Z".parse().unwrap();
        store.upsert_messages(&[late_historical]).await.unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 1,
                changed_people: 1,
                complete: true,
            }
        );
        let after_late = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap();
        assert!(after_late
            .iter()
            .any(|person| person.address == "late@example.test"));
        assert_eq!(
            after_late
                .iter()
                .find(|person| person.address == "jane+vip@example.test")
                .unwrap()
                .send_count,
            1
        );
    }

    #[tokio::test]
    async fn sent_backfill_marks_drafts_without_learning_their_recipients() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut draft = message("Unsent draft", "preview");
        draft.account_id = account_id.to_string();
        draft.id = stable_message_id(account_id, "Sent", 1);
        draft.thread_id = draft.id.clone();
        draft.mailbox = "Sent".into();
        draft.uid = 1;
        draft.is_draft = true;
        draft.to_addresses = "Never sent <draft-only@example.test>".into();
        draft.cc_addresses.clear();
        draft.bcc_addresses.clear();
        store.upsert_messages(&[draft.clone()]).await.unwrap();

        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 1,
                changed_people: 0,
                complete: true,
            }
        );
        assert!(store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap()
            .is_empty());
        let markers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_backfill_sources WHERE account_id = ? AND mailbox = 'Sent' AND uid = 1",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(markers, 1, "drafts are marked so backfill does not loop");
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 0,
                changed_people: 0,
                complete: true,
            }
        );
    }

    #[tokio::test]
    async fn sent_backfill_waits_for_recipient_headers_then_learns_final_recipients() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut sent = message("Header-only Sent", "preview");
        sent.account_id = account_id.to_string();
        sent.id = stable_message_id(account_id, "Sent", 17);
        sent.thread_id = sent.id.clone();
        sent.mailbox = "Sent".into();
        sent.uid = 17;
        sent.to_addresses = "To <to@example.test>".into();
        sent.cc_addresses.clear();
        sent.bcc_addresses.clear();
        store.upsert_messages(&[sent]).await.unwrap();
        sqlx::query("UPDATE messages SET recipient_headers_scanned = 0 WHERE account_id = ? AND mailbox = 'Sent' AND uid = 17")
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .processed_messages,
            0,
            "a header-only row must not gain a durable completed marker"
        );
        let markers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(markers, 0);

        store
            .save_recipient_headers(account_id, "Sent", 17, "", "Blind <blind@example.test>", "")
            .await
            .unwrap();
        let learned = store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();
        assert_eq!(learned.processed_messages, 1);
        let addresses = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap()
            .into_iter()
            .map(|person| person.address)
            .collect::<Vec<_>>();
        assert!(addresses.contains(&"to@example.test".to_owned()));
        assert!(addresses.contains(&"blind@example.test".to_owned()));
    }

    #[tokio::test]
    async fn sent_backfill_does_not_double_count_unmigrated_legacy_markers() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let account_key = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=501_i64 {
            let id = format!("legacy-race-{uid}");
            sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, thread_id, subject, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text) VALUES (?, ?, 'Sent', ?, NULL, ?, 'Sent', 'owner@example.test', 'Already learned <already@example.test>', '', '', '', '2026-09-01T00:00:00Z', '', '')")
                .bind(&id)
                .bind(&account_key)
                .bind(uid)
                .bind(&id)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)")
                .bind(&account_key)
                .bind(&id)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES ('already@example.test', 'Already learned', 'Already learned <already@example.test>', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z', 1, NULL)")
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count, display_name, formatted_address) VALUES ('already@example.test', ?, '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z', 1, 'Already learned', 'Already learned <already@example.test>')")
            .bind(&account_key).execute(&mut *tx).await.unwrap();
        // Model a profile with more legacy markers than its first 500-row
        // startup upgrade batch. The concurrent Sent worker must recognize
        // those old markers until the source migration publishes completion.
        sqlx::query("DELETE FROM app_meta WHERE key = ?")
            .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let first = store
            .backfill_contacted_people_from_sent(account_id, &[], 500)
            .await
            .unwrap();
        let second = store
            .backfill_contacted_people_from_sent(account_id, &[], 500)
            .await
            .unwrap();
        assert_eq!((first.changed_people, second.changed_people), (0, 0));
        let count: i64 = sqlx::query_scalar("SELECT send_count FROM contacted_people WHERE canonical_address = 'already@example.test'")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(
            count, 1,
            "legacy source migration and backfill may interleave without replaying stats"
        );
        let sources: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(sources, 501);
    }

    #[tokio::test]
    async fn unknown_uidvalidity_source_is_promoted_without_relearning_message_idless_sent_row() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut sent = message("No message id", "preview");
        sent.account_id = account_id.to_string();
        sent.id = stable_message_id(account_id, "Sent", 31);
        sent.thread_id = sent.id.clone();
        sent.mailbox = "Sent".into();
        sent.uid = 31;
        sent.message_id = None;
        sent.to_addresses = "No ID <no-id@example.test>".into();
        store.upsert_messages(&[sent]).await.unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .changed_people,
            1
        );
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 9001, 31, true)
            .await
            .unwrap();
        let after_identity = store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();
        assert_eq!(after_identity.changed_people, 0);
        let count: i64 = sqlx::query_scalar("SELECT send_count FROM contacted_people WHERE canonical_address = 'no-id@example.test'")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 1);
        let promoted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources WHERE account_id = ? AND mailbox = 'Sent' AND uid_validity = 9001 AND uid = 31")
            .bind(account_id.to_string()).fetch_one(&store.pool).await.unwrap();
        assert_eq!(promoted, 1);
    }

    #[tokio::test]
    async fn unresolved_legacy_marker_promotes_once_when_its_catalogue_row_returns() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let account_key = account_id.to_string();
        sqlx::query("INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, 'restored-legacy')")
            .bind(&account_key).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES ('legacy-restored@example.test', 'Legacy', 'Legacy <legacy-restored@example.test>', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1, NULL)")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count) VALUES ('legacy-restored@example.test', ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)")
            .bind(&account_key).execute(&store.pool).await.unwrap();
        sqlx::query("DELETE FROM app_meta WHERE key = ?")
            .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .continue_contacted_people_migrations(10)
            .await
            .unwrap();
        let mut restored = message("Restored", "preview");
        restored.id = "restored-legacy".into();
        restored.account_id = account_key.clone();
        restored.mailbox = "Sent".into();
        restored.uid = 9;
        restored.to_addresses = "Legacy <legacy-restored@example.test>".into();
        store.upsert_messages(&[restored]).await.unwrap();
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 5, 9, true)
            .await
            .unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .changed_people,
            0
        );
        let count: i64 = sqlx::query_scalar("SELECT send_count FROM contacted_people WHERE canonical_address = 'legacy-restored@example.test'")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn evicted_message_idless_legacy_marker_promotes_to_an_opaque_sent_source() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account_key = account_id.to_string();
        save_test_account(&store, account_id).await;
        let legacy_mailbox = "Sent::Archive Copy";
        let uid = 41_u32;
        store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Archive Copy".into(),
                    local_path: Some(legacy_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(71),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        // This is the real pre-v2 format. Its colon-to-underscore rewrite is
        // intentionally lossy, so recovery must rely on selectable metadata.
        let legacy_id = format!("{account_id}:{}:{uid}", legacy_mailbox.replace(':', "_"));
        sqlx::query(
            "INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)",
        )
        .bind(&account_key)
        .bind(&legacy_id)
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO contacted_people(canonical_address, display_name, formatted_address, first_contacted_at, last_contacted_at, send_count, hidden_at) VALUES ('evicted@example.test', 'Evicted', 'Evicted <evicted@example.test>', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1, NULL)")
            .execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO contacted_people_account_stats(canonical_address, account_id, first_contacted_at, last_contacted_at, send_count) VALUES ('evicted@example.test', ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)")
            .bind(&account_key).execute(&store.pool).await.unwrap();
        sqlx::query("DELETE FROM app_meta WHERE key = ?")
            .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .continue_contacted_people_migrations(10)
            .await
            .unwrap();

        let target = special_mailbox_storage_identity("Sent", "Archive Copy");
        let marker: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources WHERE account_id = ? AND mailbox = ? AND uid_validity = 71 AND uid = 41")
            .bind(&account_key).bind(&target).fetch_one(&store.pool).await.unwrap();
        assert_eq!(marker, 1);
        let mut rediscovered = message("Rediscovered", "preview");
        rediscovered.id = stable_message_id(account_id, &target, uid);
        rediscovered.account_id = account_key.clone();
        rediscovered.thread_id = rediscovered.id.clone();
        rediscovered.mailbox = target.clone();
        rediscovered.uid = i64::from(uid);
        rediscovered.message_id = None;
        rediscovered.to_addresses = "Evicted <evicted@example.test>".into();
        store.upsert_messages(&[rediscovered]).await.unwrap();
        store
            .save_mailbox_catalog_state(account_id, &target, "Archive Copy", 71, 41, true)
            .await
            .unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .changed_people,
            0
        );
        let count: i64 = sqlx::query_scalar("SELECT send_count FROM contacted_people WHERE canonical_address = 'evicted@example.test'")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn evicted_v2_markers_continue_after_selectable_path_becomes_opaque() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account_key = account_id.to_string();
        save_test_account(&store, account_id).await;
        let legacy_mailbox = "Sent::Archive Copy";
        let target = special_mailbox_storage_identity("Sent", "Archive Copy");
        store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Archive Copy".into(),
                    local_path: Some(legacy_mailbox.into()),
                    hierarchy_delimiter: None,
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Sent".into()),
                    selectable: true,
                    uid_validity: Some(72),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE + 1 {
            sqlx::query("INSERT INTO contacted_people_backfill_messages(account_id, message_id) VALUES (?, ?)")
                .bind(&account_key)
                .bind(stable_message_id(account_id, legacy_mailbox, uid as u32))
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM app_meta WHERE key IN (?, ?)")
            .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_CURSOR_KEY)
            .bind(CONTACTED_PEOPLE_SOURCE_MIGRATION_COMPLETE_KEY)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let first = store
            .continue_contacted_people_migrations(500)
            .await
            .unwrap();
        assert!(!first.complete);
        let moved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources WHERE account_id = ? AND mailbox = ?")
            .bind(&account_key).bind(&target).fetch_one(&store.pool).await.unwrap();
        assert_eq!(moved, 500);
        // This is the state after opaque mailbox migration has updated the
        // selectable local path between marker batches.
        sqlx::query("UPDATE selectable_mailboxes SET local_path = ? WHERE account_id = ? AND remote_path = 'Archive Copy'")
            .bind(&target).bind(&account_key).execute(&store.pool).await.unwrap();

        let second = store
            .continue_contacted_people_migrations(500)
            .await
            .unwrap();
        assert!(second.complete);
        let moved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacted_people_backfill_sources WHERE account_id = ? AND mailbox = ? AND uid_validity = 72")
            .bind(&account_key).bind(&target).fetch_one(&store.pool).await.unwrap();
        assert_eq!(moved, CONTACTED_PEOPLE_MIGRATION_BATCH_SIZE + 1);
    }

    #[tokio::test]
    async fn disabled_autocomplete_backfill_makes_no_people_or_marker_progress_and_fails_closed() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut first = message("First Sent", "preview");
        first.account_id = account_id.to_string();
        first.id = stable_message_id(account_id, "Sent", 1);
        first.thread_id = first.id.clone();
        first.mailbox = "Sent".into();
        first.uid = 1;
        first.to_addresses = "First <first@example.test>".into();
        first.cc_addresses.clear();
        first.bcc_addresses.clear();
        first.received_at = "2026-09-01T10:00:00Z".parse().unwrap();
        let mut second = first.clone();
        second.id = stable_message_id(account_id, "Sent", 2);
        second.thread_id = second.id.clone();
        second.uid = 2;
        second.to_addresses = "Second <second@example.test>".into();
        second.received_at = "2026-09-02T10:00:00Z".parse().unwrap();
        store.upsert_messages(&[first, second]).await.unwrap();

        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 1)
                .await
                .unwrap()
                .processed_messages,
            1
        );
        let markers_before_disable: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_backfill_messages WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        store
            .set_autocomplete_suggestions_enabled(false)
            .await
            .unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 1)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 0,
                changed_people: 0,
                complete: false,
            }
        );
        let markers_while_disabled: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_backfill_messages WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(markers_while_disabled, markers_before_disable);
        assert_eq!(
            store
                .suggest_contacted_people("", Some(account_id))
                .await
                .unwrap()
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["first@example.test"]
        );

        store
            .set_autocomplete_suggestions_enabled(true)
            .await
            .unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 1)
                .await
                .unwrap(),
            ContactedPeopleBackfillProgress {
                processed_messages: 1,
                changed_people: 0,
                complete: true,
            }
        );
        assert_eq!(
            store
                .suggest_contacted_people("", Some(account_id))
                .await
                .unwrap()
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["first@example.test"]
        );
    }

    #[tokio::test]
    async fn disabled_collection_never_learns_sent_or_smtp_recipients_from_that_interval() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .record_successful_outgoing_recipients(
                account_id,
                &[contacted_recipient(
                    "before-disable@example.test",
                    Some("Before"),
                )],
                &[],
            )
            .await
            .unwrap();
        store
            .set_autocomplete_suggestions_enabled(false)
            .await
            .unwrap();

        let mut sent_while_disabled = message("Sent while disabled", "preview");
        sent_while_disabled.account_id = account_id.to_string();
        sent_while_disabled.id = stable_message_id(account_id, "Sent", 1);
        sent_while_disabled.thread_id = sent_while_disabled.id.clone();
        sent_while_disabled.mailbox = "Sent".into();
        sent_while_disabled.uid = 1;
        sent_while_disabled.to_addresses = "Disabled <disabled@example.test>".into();
        sent_while_disabled.cc_addresses.clear();
        sent_while_disabled.bcc_addresses.clear();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_with_message_id(
                    account_id,
                    "<disabled-smtp@example.test>",
                    &[contacted_recipient(
                        "disabled-smtp@example.test",
                        Some("Disabled SMTP")
                    )],
                    &[],
                )
                .await
                .unwrap(),
            0
        );
        let outgoing_markers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_outgoing_messages WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            outgoing_markers, 1,
            "accepted Message-IDs stay suppressed while collection is disabled"
        );

        store
            .set_autocomplete_suggestions_enabled(true)
            .await
            .unwrap();
        // Simulate the next Sent SELECT after re-enabling. UID 1 existed on
        // the server during the disabled interval but is first imported only
        // now, so it must remain suppressed.
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 10, 1, true)
            .await
            .unwrap();
        store
            .capture_contacted_people_sent_provider_cutoff(account_id, "Sent", 10, 1)
            .await
            .unwrap();
        store.upsert_messages(&[sent_while_disabled]).await.unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .processed_messages,
            1
        );
        let after_reenable = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap();
        assert_eq!(
            after_reenable
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["before-disable@example.test"]
        );

        let mut sent_after_reenable = message("Sent after re-enable", "preview");
        sent_after_reenable.account_id = account_id.to_string();
        sent_after_reenable.id = stable_message_id(account_id, "Sent", 2);
        sent_after_reenable.thread_id = sent_after_reenable.id.clone();
        sent_after_reenable.mailbox = "Sent".into();
        sent_after_reenable.uid = 2;
        sent_after_reenable.to_addresses = "After <after-enable@example.test>".into();
        sent_after_reenable.cc_addresses.clear();
        sent_after_reenable.bcc_addresses.clear();
        store.upsert_messages(&[sent_after_reenable]).await.unwrap();
        store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();
        assert!(store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "after-enable@example.test"));
    }

    #[tokio::test]
    async fn smtp_message_id_marker_survives_restart_clear_and_prevents_sent_double_counting() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("contacted-people.sqlite");
        let account_id = uuid::Uuid::new_v4();
        {
            let store = Store::open(&database).await.unwrap();
            save_test_account(&store, account_id).await;
            assert_eq!(
                store
                    .record_successful_outgoing_recipients_with_message_id(
                        account_id,
                        "<smtp-accepted@example.test>",
                        &[contacted_recipient(
                            "recipient@example.test",
                            Some("Recipient")
                        )],
                        &[],
                    )
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                store
                    .record_successful_outgoing_recipients_with_message_id(
                        account_id,
                        "<smtp-accepted@example.test>",
                        &[contacted_recipient(
                            "recipient@example.test",
                            Some("Recipient")
                        )],
                        &[],
                    )
                    .await
                    .unwrap(),
                0
            );
        }

        let store = Store::open(&database).await.unwrap();
        let mut sent_copy = message("Provider Sent copy", "preview");
        sent_copy.account_id = account_id.to_string();
        sent_copy.id = stable_message_id(account_id, "Sent", 1);
        sent_copy.thread_id = sent_copy.id.clone();
        sent_copy.mailbox = "Sent".into();
        sent_copy.uid = 1;
        sent_copy.message_id = Some("<smtp-accepted@example.test>".into());
        sent_copy.to_addresses = "Recipient <recipient@example.test>".into();
        sent_copy.cc_addresses.clear();
        sent_copy.bcc_addresses.clear();
        store.upsert_messages(&[sent_copy]).await.unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .processed_messages,
            1
        );
        let learned = store
            .suggest_contacted_people("recipient", Some(account_id))
            .await
            .unwrap();
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].send_count, 1);

        store.clear_contacted_people().await.unwrap();
        let outgoing_markers_after_clear: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_outgoing_messages WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(outgoing_markers_after_clear, 1);
        assert!(store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .record_successful_outgoing_recipients_with_message_id(
                    account_id,
                    "<after-clear@example.test>",
                    &[contacted_recipient(
                        "after-clear@example.test",
                        Some("After clear")
                    )],
                    &[],
                )
                .await
                .unwrap(),
            1
        );
        store.delete_account(account_id).await.unwrap();
        let outgoing_markers_after_removal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contacted_people_outgoing_messages WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(outgoing_markers_after_removal, 0);
    }

    #[tokio::test]
    async fn sent_backfill_uidvalidity_rollover_allows_reused_uid_but_deduplicates_rfc_message_id()
    {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 1, 1, true)
            .await
            .unwrap();
        let mut original = message("Original Sent", "preview");
        original.account_id = account_id.to_string();
        original.id = stable_message_id(account_id, "Sent", 42);
        original.thread_id = original.id.clone();
        original.mailbox = "Sent".into();
        original.uid = 42;
        original.message_id = Some("<original@example.test>".into());
        original.to_addresses = "Original <original@example.test>".into();
        original.cc_addresses.clear();
        original.bcc_addresses.clear();
        store.upsert_messages(&[original]).await.unwrap();
        store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();

        store
            .reset_mailbox_catalog(account_id, "Sent")
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 2, 1, true)
            .await
            .unwrap();
        let mut replacement = message("Replacement Sent", "preview");
        replacement.account_id = account_id.to_string();
        replacement.id = stable_message_id(account_id, "Sent", 42);
        replacement.thread_id = replacement.id.clone();
        replacement.mailbox = "Sent".into();
        replacement.uid = 42;
        replacement.message_id = Some("<replacement@example.test>".into());
        replacement.to_addresses = "Replacement <replacement@example.test>".into();
        replacement.cc_addresses.clear();
        replacement.bcc_addresses.clear();
        store.upsert_messages(&[replacement.clone()]).await.unwrap();
        store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();
        assert!(store
            .suggest_contacted_people("replacement", Some(account_id))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "replacement@example.test"));

        store
            .reset_mailbox_catalog(account_id, "Sent")
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 3, 1, true)
            .await
            .unwrap();
        store.upsert_messages(&[replacement]).await.unwrap();
        let repeat = store
            .backfill_contacted_people_from_sent(account_id, &[], 10)
            .await
            .unwrap();
        assert_eq!(repeat.changed_people, 0);
        assert_eq!(
            store
                .suggest_contacted_people("replacement", Some(account_id))
                .await
                .unwrap()[0]
                .send_count,
            1
        );
    }

    #[tokio::test]
    async fn account_removal_keeps_other_people_contributions_and_clear_removes_history() {
        let store = Store::in_memory().await.unwrap();
        let first_account = uuid::Uuid::new_v4();
        let second_account = uuid::Uuid::new_v4();
        save_test_account(&store, first_account).await;
        save_test_account(&store, second_account).await;
        for (account_id, display_name) in [
            (first_account, "Work contact"),
            (second_account, "Personal contact"),
        ] {
            store
                .record_successful_outgoing_recipients(
                    account_id,
                    &[contacted_recipient(
                        "shared@example.test",
                        Some(display_name),
                    )],
                    &[],
                )
                .await
                .unwrap();
        }
        store
            .record_successful_outgoing_recipients(
                first_account,
                &[contacted_recipient(
                    "only-first@example.test",
                    Some("Only first"),
                )],
                &[],
            )
            .await
            .unwrap();
        store.delete_account(first_account).await.unwrap();
        let retained = store
            .suggest_contacted_people("", Some(second_account))
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].address, "shared@example.test");
        assert_eq!(retained[0].send_count, 1);
        assert_eq!(retained[0].account_send_count, 1);
        assert_eq!(
            retained[0].display_name.as_deref(),
            Some("Personal contact")
        );
        assert_eq!(
            retained[0].formatted_address,
            "Personal contact <shared@example.test>"
        );
        store.clear_contacted_people().await.unwrap();
        assert!(store
            .suggest_contacted_people("", Some(second_account))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn adding_an_account_hides_its_existing_address_from_contact_suggestions() {
        let store = Store::in_memory().await.unwrap();
        let sender = uuid::Uuid::new_v4();
        save_test_account(&store, sender).await;
        store
            .record_successful_outgoing_recipients(
                sender,
                &[contacted_recipient(
                    "later-owner@example.test",
                    Some("Later owner"),
                )],
                &[],
            )
            .await
            .unwrap();
        assert!(store
            .suggest_contacted_people("later-owner", Some(sender))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "later-owner@example.test"));

        let second = uuid::Uuid::new_v4();
        store
            .save_account(&account_with_id(second, "later-owner@example.test"))
            .await
            .unwrap();
        assert!(!store
            .suggest_contacted_people("later-owner", Some(sender))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "later-owner@example.test"));
    }

    #[tokio::test]
    async fn unicode_owner_address_added_after_learning_is_excluded_from_suggestions() {
        let store = Store::in_memory().await.unwrap();
        let sender = uuid::Uuid::new_v4();
        save_test_account(&store, sender).await;
        store
            .record_successful_outgoing_recipients(
                sender,
                &[contacted_recipient("münich@example.test", Some("Münich"))],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .suggest_contacted_people("münich", Some(sender))
                .await
                .unwrap()
                .len(),
            1
        );

        store
            .save_account(&account_with_id(
                uuid::Uuid::new_v4(),
                "MÜNICH@example.test",
            ))
            .await
            .unwrap();
        assert!(store
            .suggest_contacted_people("münich", Some(sender))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn account_removal_recomputes_contacted_people_from_enabled_accounts_only() {
        let store = Store::in_memory().await.unwrap();
        let removed = uuid::Uuid::new_v4();
        let enabled = uuid::Uuid::new_v4();
        let disabled = uuid::Uuid::new_v4();
        save_test_account(&store, removed).await;
        save_test_account(&store, enabled).await;
        save_test_account(&store, disabled).await;
        let recipient = contacted_recipient("aggregate@example.test", Some("Enabled"));
        store
            .record_successful_outgoing_recipients(removed, std::slice::from_ref(&recipient), &[])
            .await
            .unwrap();
        store
            .record_successful_outgoing_recipients(enabled, std::slice::from_ref(&recipient), &[])
            .await
            .unwrap();
        for _ in 0..3 {
            store
                .record_successful_outgoing_recipients(
                    disabled,
                    &[contacted_recipient(
                        "aggregate@example.test",
                        Some("Disabled"),
                    )],
                    &[],
                )
                .await
                .unwrap();
        }
        let mut disabled_account = store.account(disabled).await.unwrap().unwrap();
        disabled_account.enabled = false;
        store.save_account(&disabled_account).await.unwrap();

        store.delete_account(removed).await.unwrap();
        let suggestion = store
            .suggest_contacted_people("aggregate", Some(enabled))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(suggestion.send_count, 1);
        assert_eq!(suggestion.display_name.as_deref(), Some("Enabled"));
        let enabled_id = enabled.to_string();
        assert_eq!(suggestion.account_id.as_deref(), Some(enabled_id.as_str()));
        assert_eq!(
            store
                .suggest_contacted_people("aggregate", None)
                .await
                .unwrap()
                .remove(0)
                .account_id,
            None,
            "a global fallback has no preferred-account contribution"
        );
    }

    #[tokio::test]
    async fn disabled_account_contributions_are_hidden_and_reappear_when_reenabled() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .record_successful_outgoing_recipients(
                account_id,
                &[contacted_recipient(
                    "paused-account@example.test",
                    Some("Paused"),
                )],
                &[],
            )
            .await
            .unwrap();
        let mut account = store.account(account_id).await.unwrap().unwrap();
        account.enabled = false;
        store.save_account(&account).await.unwrap();
        assert!(store
            .suggest_contacted_people("paused-account", None)
            .await
            .unwrap()
            .is_empty());
        account.enabled = true;
        store.save_account(&account).await.unwrap();
        assert_eq!(
            store
                .suggest_contacted_people("paused-account", Some(account_id))
                .await
                .unwrap()[0]
                .send_count,
            1
        );
    }

    #[tokio::test]
    async fn suggestions_rank_and_report_only_enabled_account_contributions() {
        let store = Store::in_memory().await.unwrap();
        let active = uuid::Uuid::new_v4();
        let disabled = uuid::Uuid::new_v4();
        save_test_account(&store, active).await;
        save_test_account(&store, disabled).await;
        let person = contacted_recipient("mixed-history@example.test", Some("Mixed history"));
        store
            .record_successful_outgoing_recipients(active, std::slice::from_ref(&person), &[])
            .await
            .unwrap();
        for _ in 0..3 {
            store
                .record_successful_outgoing_recipients(disabled, std::slice::from_ref(&person), &[])
                .await
                .unwrap();
        }
        let mut disabled_account = store.account(disabled).await.unwrap().unwrap();
        disabled_account.enabled = false;
        store.save_account(&disabled_account).await.unwrap();

        let suggestion = store
            .suggest_contacted_people("mixed-history", Some(active))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(suggestion.send_count, 1);
        assert_eq!(suggestion.account_send_count, 1);

        disabled_account.enabled = true;
        store.save_account(&disabled_account).await.unwrap();
        assert_eq!(
            store
                .suggest_contacted_people("mixed-history", Some(active))
                .await
                .unwrap()[0]
                .send_count,
            4,
            "disabled contributions remain local and return only after re-enable"
        );
    }

    #[tokio::test]
    async fn disabling_an_account_rebuilds_active_display_name_match_tokens() {
        let store = Store::in_memory().await.unwrap();
        let alice_account = uuid::Uuid::new_v4();
        let bob_account = uuid::Uuid::new_v4();
        save_test_account(&store, alice_account).await;
        save_test_account(&store, bob_account).await;
        let recipient = "shared-name@example.test";
        store
            .record_successful_outgoing_recipients(
                bob_account,
                &[contacted_recipient(recipient, Some("Bob Active"))],
                &[],
            )
            .await
            .unwrap();
        store
            .record_successful_outgoing_recipients(
                alice_account,
                &[contacted_recipient(recipient, Some("Alice Disabled"))],
                &[],
            )
            .await
            .unwrap();
        let mut alice = store.account(alice_account).await.unwrap().unwrap();
        alice.enabled = false;
        store.save_account(&alice).await.unwrap();

        assert!(store
            .suggest_contacted_people("alice", Some(bob_account))
            .await
            .unwrap()
            .is_empty());
        let active = store
            .suggest_contacted_people("bob", Some(bob_account))
            .await
            .unwrap();
        assert_eq!(active[0].address, recipient);
        assert_eq!(active[0].display_name.as_deref(), Some("Bob Active"));
        assert_eq!(active[0].send_count, 1);
    }

    #[tokio::test]
    async fn later_hide_and_clear_win_over_an_earlier_accepted_send() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let recipient = contacted_recipient("ordered@example.test", Some("Ordered"));
        store
            .record_successful_outgoing_recipients(
                account_id,
                std::slice::from_ref(&recipient),
                &[],
            )
            .await
            .unwrap();
        let accepted_before_hide = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        store
            .hide_contacted_person("ordered@example.test")
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_before_hide,
                )
                .await
                .unwrap(),
            1,
            "the send may be counted, but a later hide remains visible state"
        );
        assert!(store
            .suggest_contacted_people("ordered", Some(account_id))
            .await
            .unwrap()
            .is_empty());

        let accepted_after_hide = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_after_hide,
                )
                .await
                .unwrap(),
            1
        );
        assert!(store
            .suggest_contacted_people("ordered", Some(account_id))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "ordered@example.test"));

        let accepted_before_clear = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        store.clear_contacted_people().await.unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_before_clear,
                )
                .await
                .unwrap(),
            0,
            "a send accepted before Clear cannot repopulate history"
        );
        let accepted_after_clear = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_after_clear,
                )
                .await
                .unwrap(),
            1,
            "a later accepted send starts a new local history"
        );
        assert!(store
            .suggest_contacted_people("ordered", Some(account_id))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "ordered@example.test"));
    }

    #[tokio::test]
    async fn contacted_people_action_sequences_make_clear_and_hide_causal() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let recipient = contacted_recipient("sequenced@example.test", Some("Sequenced"));
        let accepted_before_clear = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        store.clear_contacted_people().await.unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_before_clear
                )
                .await
                .unwrap(),
            0
        );
        let accepted_after_clear = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_after_clear
                )
                .await
                .unwrap(),
            1
        );
        store
            .hide_contacted_person("sequenced@example.test")
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_after_clear
                )
                .await
                .unwrap(),
            1
        );
        assert!(store
            .suggest_contacted_people("sequenced", Some(account_id))
            .await
            .unwrap()
            .is_empty());
        let accepted_after_hide = store
            .reserve_contacted_people_action_sequence()
            .await
            .unwrap();
        assert_eq!(
            store
                .record_successful_outgoing_recipients_at_sequence(
                    account_id,
                    std::slice::from_ref(&recipient),
                    &[],
                    accepted_after_hide
                )
                .await
                .unwrap(),
            1
        );
        assert!(store
            .suggest_contacted_people("sequenced", Some(account_id))
            .await
            .unwrap()
            .iter()
            .any(|person| person.address == "sequenced@example.test"));
    }

    #[tokio::test]
    async fn autocomplete_suggestions_setting_defaults_to_enabled_and_persists() {
        let store = Store::in_memory().await.unwrap();
        assert!(store.autocomplete_suggestions_enabled().await.unwrap());
        store
            .set_autocomplete_suggestions_enabled(false)
            .await
            .unwrap();
        assert!(!store.autocomplete_suggestions_enabled().await.unwrap());
        store
            .set_autocomplete_suggestions_enabled(true)
            .await
            .unwrap();
        assert!(store.autocomplete_suggestions_enabled().await.unwrap());
    }

    #[test]
    fn contacted_person_suggestion_uses_the_tauri_snake_case_payload_contract() {
        let suggestion = ContactedPersonSuggestion {
            address: "recipient@example.test".into(),
            display_name: Some("Recipient".into()),
            formatted_address: "Recipient <recipient@example.test>".into(),
            first_contacted_at: "2026-09-01T10:00:00Z".parse().unwrap(),
            last_contacted_at: "2026-09-02T10:00:00Z".parse().unwrap(),
            send_count: 3,
            account_send_count: 2,
            account_last_contacted_at: Some("2026-09-02T10:00:00Z".parse().unwrap()),
            account_id: Some("account-1".into()),
            hidden: false,
        };
        let value = serde_json::to_value(suggestion).unwrap();
        let mut keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "account_id",
                "account_last_contacted_at",
                "account_send_count",
                "address",
                "display_name",
                "first_contacted_at",
                "formatted_address",
                "hidden",
                "last_contacted_at",
                "send_count",
            ]
        );
        assert!(value.get("formattedAddress").is_none());
    }

    #[tokio::test]
    async fn clear_history_uses_provider_cutoff_across_restart_and_allows_new_sent_mail() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("contacted-people.sqlite");
        let account_id = uuid::Uuid::new_v4();
        {
            let store = Store::open(&database).await.unwrap();
            save_test_account(&store, account_id).await;
            let mut previously_backfilled = message("Previously backfilled", "preview");
            previously_backfilled.account_id = account_id.to_string();
            previously_backfilled.id = stable_message_id(account_id, "Sent", 1);
            previously_backfilled.thread_id = previously_backfilled.id.clone();
            previously_backfilled.mailbox = "Sent".into();
            previously_backfilled.uid = 1;
            previously_backfilled.to_addresses = "Known <known@example.test>".into();
            previously_backfilled.cc_addresses.clear();
            previously_backfilled.bcc_addresses.clear();
            previously_backfilled.received_at = "2020-01-01T10:00:00Z".parse().unwrap();
            let mut future_dated_before_clear = message("Future-dated before clear", "preview");
            future_dated_before_clear.account_id = account_id.to_string();
            future_dated_before_clear.id = stable_message_id(account_id, "Sent", 3);
            future_dated_before_clear.thread_id = future_dated_before_clear.id.clone();
            future_dated_before_clear.mailbox = "Sent".into();
            future_dated_before_clear.uid = 3;
            future_dated_before_clear.to_addresses = "Future <future@example.test>".into();
            future_dated_before_clear.cc_addresses.clear();
            future_dated_before_clear.bcc_addresses.clear();
            future_dated_before_clear.received_at = "2999-01-01T00:00:00Z".parse().unwrap();
            store
                .upsert_messages(&[previously_backfilled, future_dated_before_clear])
                .await
                .unwrap();
            assert_eq!(
                store
                    .backfill_contacted_people_from_sent(account_id, &[], 1)
                    .await
                    .unwrap()
                    .processed_messages,
                1
            );
            store.clear_contacted_people().await.unwrap();
            assert!(store
                .suggest_contacted_people("", Some(account_id))
                .await
                .unwrap()
                .is_empty());
        }

        let store = Store::open(&database).await.unwrap();
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 10, 3, true)
            .await
            .unwrap();
        store
            .capture_contacted_people_sent_provider_cutoff(account_id, "Sent", 10, 3)
            .await
            .unwrap();
        // This old server UID was not locally catalogued until after Clear.
        // A trusted server cutoff, not the Date header or discovery time,
        // keeps it private when it is eventually imported.
        let mut old_server_message = message("Old server Sent", "preview");
        old_server_message.account_id = account_id.to_string();
        old_server_message.id = stable_message_id(account_id, "Sent", 2);
        old_server_message.thread_id = old_server_message.id.clone();
        old_server_message.mailbox = "Sent".into();
        old_server_message.uid = 2;
        old_server_message.to_addresses = "Old <old@example.test>".into();
        old_server_message.cc_addresses.clear();
        old_server_message.bcc_addresses.clear();
        old_server_message.received_at = "2020-01-02T10:00:00Z".parse().unwrap();
        store.upsert_messages(&[old_server_message]).await.unwrap();
        // UID 1 was processed before this mailbox had a trusted UIDVALIDITY.
        // Scan it once more to attach the provider identity marker, while the
        // RFC Message-ID marker prevents its recipient from being relearned.
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .processed_messages,
            2
        );
        assert!(store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap()
            .is_empty());

        // A Sent message discovered after Clear is eligible even if a remote
        // client supplied an old Date. The local catalogue timestamp, rather
        // than that provider-controlled Date, is the privacy boundary.
        let mut newly_catalogued_after_clear = message("Newly discovered Sent", "preview");
        newly_catalogued_after_clear.account_id = account_id.to_string();
        newly_catalogued_after_clear.id = stable_message_id(account_id, "Sent", 4);
        newly_catalogued_after_clear.thread_id = newly_catalogued_after_clear.id.clone();
        newly_catalogued_after_clear.mailbox = "Sent".into();
        newly_catalogued_after_clear.uid = 4;
        newly_catalogued_after_clear.to_addresses = "New client <new-client@example.test>".into();
        newly_catalogued_after_clear.cc_addresses.clear();
        newly_catalogued_after_clear.bcc_addresses.clear();
        newly_catalogued_after_clear.received_at = "2000-01-01T00:00:00Z".parse().unwrap();
        store
            .upsert_messages(&[newly_catalogued_after_clear])
            .await
            .unwrap();
        assert_eq!(
            store
                .backfill_contacted_people_from_sent(account_id, &[], 10)
                .await
                .unwrap()
                .processed_messages,
            1
        );
        assert_eq!(
            store
                .suggest_contacted_people("", Some(account_id))
                .await
                .unwrap()
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["new-client@example.test"]
        );
        store
            .record_successful_outgoing_recipients(
                account_id,
                &[contacted_recipient("manual@example.test", Some("Manual"))],
                &[],
            )
            .await
            .unwrap();
        let after_send = store
            .suggest_contacted_people("", Some(account_id))
            .await
            .unwrap();
        assert_eq!(
            after_send
                .iter()
                .map(|person| person.address.as_str())
                .collect::<Vec<_>>(),
            vec!["manual@example.test", "new-client@example.test"]
        );
    }

    fn fixed_seed_order(len: usize, mut seed: u64) -> Vec<usize> {
        let mut order = (0..len).collect::<Vec<_>>();
        for index in (1..len).rev() {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            order.swap(index, (seed as usize) % (index + 1));
        }
        order
    }

    #[tokio::test]
    async fn recipient_headers_round_trip_and_refresh_without_inference() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Recipients", "preview");
        message.to_addresses = "Primary <primary@example.test>".into();
        message.cc_addresses = "Cc <cc@example.test>".into();
        message.bcc_addresses = "Hidden <hidden@example.test>".into();
        message.reply_to_addresses = "Replies <replies@example.test>".into();
        let id = message.id.clone();
        let account_id = uuid::Uuid::parse_str(&message.account_id).unwrap();

        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        let stored = store.message(&id).await.unwrap().unwrap();
        assert_eq!(stored.cc_addresses, "Cc <cc@example.test>");
        assert_eq!(stored.bcc_addresses, "Hidden <hidden@example.test>");
        assert_eq!(stored.reply_to_addresses, "Replies <replies@example.test>");
        assert!(store
            .unscanned_recipient_header_uids(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_empty());

        message.cc_addresses.clear();
        message.bcc_addresses.clear();
        message.reply_to_addresses.clear();
        store.upsert_messages(&[message]).await.unwrap();
        let refreshed = store.message(&id).await.unwrap().unwrap();
        assert!(refreshed.cc_addresses.is_empty());
        assert!(refreshed.bcc_addresses.is_empty());
        assert!(refreshed.reply_to_addresses.is_empty());
    }

    #[tokio::test]
    async fn body_cache_round_trips_metadata_and_touches_lru() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Cached", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();
        let content = CachedMessageContent {
            body_text: "Cached body".into(),
            body_html: Some("<p>Cached body</p>".into()),
            unsubscribe_kind: Some("one_click".into()),
            attachments: vec![Attachment {
                id: "metadata-only".into(),
                message_id: "wrong-local-id".into(),
                filename: "invoice.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 42,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            }],
        };
        store
            .cache_message_content(&id, false, content)
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar(
            "SELECT last_accessed FROM message_content_cache WHERE message_id = ?",
        )
        .bind(&id)
        .fetch_one(&store.pool)
        .await
        .unwrap();

        let cached = store.cached_message_content(&id).await.unwrap().unwrap();
        assert_eq!(cached.body_text, "Cached body");
        assert_eq!(cached.body_html.as_deref(), Some("<p>Cached body</p>"));
        assert_eq!(cached.unsubscribe_kind.as_deref(), Some("one_click"));
        assert_eq!(cached.attachments.len(), 1);
        assert_eq!(cached.attachments[0].message_id, id);
        let after: i64 = sqlx::query_scalar(
            "SELECT last_accessed FROM message_content_cache WHERE message_id = ?",
        )
        .bind(&id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(after > before);
        let attachment_bytes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(attachment_bytes, 0);
    }

    #[tokio::test]
    async fn attachment_filename_cache_migration_discards_stale_foreground_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("filename-cache-migration.sqlite");
        let store = Store::open(&database).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut cached_message = message("Filename migration", "preview");
        cached_message.account_id = account_id.to_string();
        let id = cached_message.id.clone();
        store.upsert_messages(&[cached_message]).await.unwrap();
        store
            .cache_message_content(
                &id,
                false,
                CachedMessageContent {
                    body_text: "cached body".into(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: vec![Attachment {
                        id: "stale-filename".into(),
                        message_id: id.clone(),
                        filename: "=UTF-8QPr=C3=BCgimaja_plaan.pdf=".into(),
                        mime_type: "application/pdf".into(),
                        size_bytes: 42,
                        is_inline: false,
                        presentation: AttachmentPresentation::Downloadable,
                        is_potentially_unsafe: false,
                    }],
                },
            )
            .await
            .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_some());

        let mut starred = message("Starred filename migration", "preview");
        starred.account_id = account_id.to_string();
        starred.uid = 2;
        starred.is_flagged = true;
        starred.has_attachments = true;
        let starred_id = starred.id.clone();
        store.upsert_messages(&[starred]).await.unwrap();
        assert!(store
            .cache_starred_message_content(
                &starred_id,
                CachedMessageContent {
                    body_text: "starred cached body".into(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: vec![Attachment {
                        id: "stale-starred-filename".into(),
                        message_id: starred_id.clone(),
                        filename: "=UTF-8QPr=C3=BCgimaja_plaan.pdf=".into(),
                        mime_type: "application/pdf".into(),
                        size_bytes: 42,
                        is_inline: false,
                        presentation: AttachmentPresentation::Downloadable,
                        is_potentially_unsafe: false,
                    }],
                },
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .starred_attachment_metadata(&starred_id)
                .await
                .unwrap()
                .len(),
            1
        );

        // Version 2 predates selective RFC 2047 filename decoding. Reopening
        // takes the production migration path and forces the authoritative
        // provider fetch instead of showing a cached transport artifact.
        sqlx::query(
            "UPDATE app_meta SET value = '2' WHERE key = 'attachment_presentation_cache_version'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE starred_message_bodies SET attachment_presentation_version = 2 WHERE message_id = ?",
        )
        .bind(&starred_id)
        .execute(&store.pool)
        .await
        .unwrap();
        store.pool.close().await;

        let store = Store::open(&database).await.unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        let cached_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache WHERE message_id = ?")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(cached_rows, 0);
        assert!(!store.message(&id).await.unwrap().unwrap().has_attachments);
        assert!(store.starred_body(&starred_id).await.unwrap().is_none());
        assert!(store
            .starred_attachment_metadata(&starred_id)
            .await
            .unwrap()
            .is_empty());
        assert!(
            !store
                .message(&starred_id)
                .await
                .unwrap()
                .unwrap()
                .has_attachments
        );
    }

    #[tokio::test]
    async fn attachment_presentation_migration_invalidates_starred_body_until_refetch_restores_real_files(
    ) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.unwrap();
        let account = AccountDraft {
            email: "attachment-migration@example.test".into(),
            display_name: "Attachment migration".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let mut legacy = message("Claim", "stale body");
        legacy.account_id = account.id.to_string();
        legacy.is_flagged = true;
        legacy.has_attachments = true;
        legacy.attachments.push(AttachmentData {
            attachment: Attachment {
                id: "legacy-pdf".into(),
                message_id: legacy.id.clone(),
                filename: "claim.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 3,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            },
            bytes: b"pdf".to_vec(),
        });
        let id = legacy.id.clone();
        store.upsert_messages(&[legacy.clone()]).await.unwrap();
        assert!(store.starred_body(&id).await.unwrap().is_some());
        assert_eq!(
            store.starred_attachment_metadata(&id).await.unwrap().len(),
            1
        );

        // Simulate version 1, whose full-parser downloadable-only attachment
        // indexes can point at a different MIME part under the sectioned
        // planner, then reopen so migration follows the production path.
        sqlx::query(
            "UPDATE app_meta SET value = '1' WHERE key = 'attachment_presentation_cache_version'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("UPDATE starred_message_bodies SET attachment_presentation_version = 1 WHERE message_id = ?")
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        let store = Store::open(&database).await.unwrap();

        // Durable text survives migration, but the public cache API refuses
        // to return it with stale attachment metadata.
        let durable_body: String =
            sqlx::query_scalar("SELECT body_text FROM starred_message_bodies WHERE message_id = ?")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(durable_body, "stale body");
        assert!(store.starred_body(&id).await.unwrap().is_none());
        assert!(store
            .starred_attachment_metadata(&id)
            .await
            .unwrap()
            .is_empty());
        assert!(!store.message(&id).await.unwrap().unwrap().has_attachments);

        // Header-only catalogue publication cannot blank durable starred text
        // or mark the stale body fresh.
        let mut catalogue = legacy.clone();
        catalogue.content_state = "headers_only".into();
        catalogue.body_text.clear();
        catalogue.body_html = None;
        catalogue.has_attachments = false;
        catalogue.attachments.clear();
        store.upsert_catalog_messages(&[catalogue]).await.unwrap();
        let durable_after_catalogue: String =
            sqlx::query_scalar("SELECT body_text FROM starred_message_bodies WHERE message_id = ?")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(durable_after_catalogue, "stale body");
        assert!(store.starred_body(&id).await.unwrap().is_none());

        // The authoritative fetch path can update exactly this message's
        // paperclip state and refresh the starred cache without touching flags.
        assert!(store
            .update_message_attachment_state(&id, true)
            .await
            .unwrap());
        assert!(store
            .cache_starred_message_content(
                &id,
                CachedMessageContent {
                    body_text: legacy.body_text.clone(),
                    body_html: legacy.body_html.clone(),
                    unsubscribe_kind: legacy.unsubscribe_kind.clone(),
                    attachments: legacy
                        .attachments
                        .iter()
                        .map(|attachment| attachment.attachment.clone())
                        .collect(),
                },
            )
            .await
            .unwrap());
        assert!(store.message(&id).await.unwrap().unwrap().has_attachments);
        assert_eq!(
            store.starred_body(&id).await.unwrap().unwrap().0,
            "stale body"
        );
        assert_eq!(
            store.starred_attachment_metadata(&id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn authoritative_content_cache_never_revives_an_unstarred_or_missing_message() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Current", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();
        let content = CachedMessageContent {
            body_text: "authoritative body".into(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: vec![Attachment {
                id: "foreign-attachment".into(),
                message_id: "other-message".into(),
                filename: "claim.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 3,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            }],
        };

        assert!(!store
            .cache_starred_message_content(&id, content.clone())
            .await
            .unwrap());
        assert!(store.starred_body(&id).await.unwrap().is_none());
        assert!(!store
            .update_message_attachment_state("missing-message", true)
            .await
            .unwrap());

        store.set_message_flagged(&id, true).await.unwrap();
        assert!(store
            .cache_starred_message_content(&id, content)
            .await
            .unwrap());
        let attachment = store
            .starred_attachment_metadata(&id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(attachment.message_id, id);
        assert_eq!(attachment.filename, "claim.pdf");
    }

    #[tokio::test]
    async fn cached_legacy_attachment_metadata_is_deleted_instead_of_guessed_from_inline() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Legacy", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();
        sqlx::query("INSERT INTO message_content_cache(message_id, content_state, body_text, body_html, unsubscribe_kind, attachments_json, byte_size, last_accessed) VALUES (?, 'complete', 'old', NULL, NULL, ?, 3, 1)")
            .bind(&id)
            .bind(serde_json::json!([{
                "id": "old-inline",
                "message_id": id,
                "filename": "image001.png",
                "mime_type": "image/png",
                "size_bytes": 7,
                "is_inline": true,
                "is_potentially_unsafe": false
            }]).to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache WHERE message_id = ?")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn body_cache_replaces_accounting_and_uses_global_byte_lru() {
        let store = Store::in_memory().await.unwrap();
        let first = message("First", "preview");
        let first_id = first.id.clone();
        let mut second = message("Second", "preview");
        second.account_id = first.account_id.clone();
        second.uid = 2;
        let second_id = second.id.clone();
        store.upsert_messages(&[first, second]).await.unwrap();

        store
            .cache_message_content(&first_id, false, cached_content("éééé"))
            .await
            .unwrap();
        store
            .cache_message_content(&first_id, false, cached_content("small"))
            .await
            .unwrap();
        let expected = cache_entry_byte_size("small", None, None, "[]").unwrap();
        let used: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM message_content_cache")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(used, expected);

        for index in 0..=64 {
            let mut entry = message("Entry", "preview");
            entry.id = format!("entry-{index}");
            entry.account_id = "cache-entry-account".into();
            entry.uid = index + 10;
            store.upsert_messages(&[entry]).await.unwrap();
            store
                .cache_message_content(
                    &format!("entry-{index}"),
                    false,
                    cached_content(&format!("entry-{index}")),
                )
                .await
                .unwrap();
        }
        assert!(store
            .cached_message_content(&first_id)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .cached_message_content(&second_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content("entry-64")
            .await
            .unwrap()
            .is_some());
        let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(entries > 64);

        let byte_store = Store::in_memory().await.unwrap();
        let byte_first = message("Byte first", "preview");
        let byte_first_id = byte_first.id.clone();
        let mut byte_second = message("Byte second", "preview");
        byte_second.account_id = byte_first.account_id.clone();
        byte_second.uid = 2;
        let byte_second_id = byte_second.id.clone();
        byte_store
            .upsert_messages(&[byte_first, byte_second])
            .await
            .unwrap();
        byte_store
            .cache_message_content(&byte_first_id, false, cached_content("first"))
            .await
            .unwrap();
        byte_store
            .cache_message_content(&byte_second_id, false, cached_content("second"))
            .await
            .unwrap();
        sqlx::query("UPDATE message_content_cache SET byte_size = ?")
            .bind(MESSAGE_CONTENT_CACHE_MAX_BYTES / 2)
            .execute(&byte_store.pool)
            .await
            .unwrap();
        let mut byte_third = message("Byte third", "preview");
        byte_third.account_id = byte_store
            .message(&byte_second_id)
            .await
            .unwrap()
            .unwrap()
            .account_id;
        byte_third.uid = 3;
        let byte_third_id = byte_third.id.clone();
        byte_store.upsert_messages(&[byte_third]).await.unwrap();
        byte_store
            .cache_message_content(&byte_third_id, false, cached_content("third"))
            .await
            .unwrap();
        assert!(byte_store
            .cached_message_content(&byte_first_id)
            .await
            .unwrap()
            .is_none());
        assert!(byte_store
            .cached_message_content(&byte_second_id)
            .await
            .unwrap()
            .is_some());
        assert!(byte_store
            .cached_message_content(&byte_third_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn recent_body_cache_candidates_respect_cache_folder_cutoff_and_exact_message_id_dedupe()
    {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let cutoff = Utc::now();
        let mut duplicate_new = cache_candidate_message(
            account_id,
            "duplicate-new",
            1,
            "INBOX",
            cutoff + chrono::Duration::seconds(10),
        );
        duplicate_new.message_id = Some("<duplicate@example.test>".into());
        let mut duplicate_old = cache_candidate_message(
            account_id,
            "duplicate-old",
            2,
            "Sent",
            cutoff + chrono::Duration::seconds(9),
        );
        duplicate_old.message_id = duplicate_new.message_id.clone();
        let mut flagged_missing = cache_candidate_message(
            account_id,
            "flagged-missing",
            3,
            "INBOX",
            cutoff + chrono::Duration::seconds(8),
        );
        flagged_missing.is_flagged = true;
        flagged_missing.content_state = "headers_only".into();
        let sent = cache_candidate_message(
            account_id,
            "sent-candidate",
            4,
            "Sent",
            cutoff + chrono::Duration::seconds(7),
        );
        let archive = cache_candidate_message(
            account_id,
            "archive-candidate",
            5,
            "Archive",
            cutoff + chrono::Duration::seconds(6),
        );
        let cached_regular = cache_candidate_message(
            account_id,
            "cached-regular",
            6,
            "INBOX",
            cutoff + chrono::Duration::seconds(5),
        );
        let mut cached_starred = cache_candidate_message(
            account_id,
            "cached-starred",
            7,
            "INBOX",
            cutoff + chrono::Duration::seconds(4),
        );
        cached_starred.is_flagged = true;
        let boundary = cache_candidate_message(account_id, "cutoff-boundary", 8, "INBOX", cutoff);
        let old = cache_candidate_message(
            account_id,
            "older-than-cutoff",
            9,
            "INBOX",
            cutoff - chrono::Duration::nanoseconds(1),
        );
        let draft = cache_candidate_message(
            account_id,
            "draft-excluded",
            10,
            "Drafts",
            cutoff + chrono::Duration::seconds(20),
        );
        store
            .upsert_messages(&[
                duplicate_new.clone(),
                duplicate_old,
                flagged_missing.clone(),
                sent.clone(),
                archive.clone(),
                cached_regular.clone(),
                cached_starred.clone(),
                boundary.clone(),
                old,
                draft,
            ])
            .await
            .unwrap();
        store
            .cache_message_content(&cached_regular.id, false, cached_content("regular"))
            .await
            .unwrap();
        assert!(store
            .cache_starred_message_content(&cached_starred.id, cached_content("starred"))
            .await
            .unwrap());

        let candidates = store
            .recent_body_cache_candidates(account_id, cutoff, 20)
            .await
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            [
                duplicate_new.id.as_str(),
                flagged_missing.id.as_str(),
                sent.id.as_str(),
                archive.id.as_str(),
                boundary.id.as_str(),
            ]
        );
        for (offset, expected) in [
            (
                0,
                vec![duplicate_new.id.as_str(), flagged_missing.id.as_str()],
            ),
            (2, vec![sent.id.as_str(), archive.id.as_str()]),
            (4, vec![boundary.id.as_str()]),
        ] {
            let page = store
                .recent_body_cache_candidates_page(account_id, cutoff, 2, offset)
                .await
                .unwrap();
            assert_eq!(
                page.iter()
                    .map(|message| message.id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn body_cache_evicts_non_recent_entries_before_recent_primary_lru() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let old_primary = cache_candidate_message(
            account_id,
            "old-primary",
            1,
            "INBOX",
            now - chrono::Duration::days(MESSAGE_CONTENT_CACHE_RECENT_WINDOW_DAYS + 1),
        );
        let non_primary = cache_candidate_message(account_id, "draft", 2, "Drafts", now);
        let recent_primary = cache_candidate_message(account_id, "recent-primary", 3, "Sent", now);
        let incoming = cache_candidate_message(account_id, "incoming", 4, "Archive", now);
        store
            .upsert_messages(&[
                old_primary.clone(),
                non_primary.clone(),
                recent_primary.clone(),
                incoming.clone(),
            ])
            .await
            .unwrap();
        for message in [&old_primary, &non_primary, &recent_primary] {
            store
                .cache_message_content(&message.id, false, cached_content(&message.id))
                .await
                .unwrap();
        }
        for (id, byte_size, last_accessed) in [
            (
                old_primary.id.as_str(),
                MESSAGE_CONTENT_CACHE_MAX_BYTES / 2 + 1,
                2,
            ),
            (
                non_primary.id.as_str(),
                MESSAGE_CONTENT_CACHE_MAX_BYTES / 2 - 1,
                3,
            ),
            (recent_primary.id.as_str(), 1, 1),
        ] {
            sqlx::query("UPDATE message_content_cache SET byte_size = ?, last_accessed = ? WHERE message_id = ?")
                .bind(byte_size)
                .bind(last_accessed)
                .bind(id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        store
            .cache_message_content(&incoming.id, false, cached_content("incoming"))
            .await
            .unwrap();

        assert!(store
            .cached_message_content(&old_primary.id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&non_primary.id)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .cached_message_content(&recent_primary.id)
            .await
            .unwrap()
            .is_some());
        let used: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM message_content_cache")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(used <= MESSAGE_CONTENT_CACHE_MAX_BYTES);
    }

    #[tokio::test]
    async fn message_content_fetch_claims_distinguish_busy_and_missing_messages() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Claim", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();

        let claim = match store
            .acquire_message_content_fetch_outcome(&id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("first reader must own the claim")
            }
        };
        assert!(matches!(
            store
                .acquire_message_content_fetch_outcome(&id)
                .await
                .unwrap(),
            MessageContentFetchAcquire::Busy
        ));
        assert!(matches!(
            store
                .acquire_message_content_fetch_outcome("missing-message")
                .await
                .unwrap(),
            MessageContentFetchAcquire::Missing
        ));
        claim.release().await.unwrap();
        let reacquired = match store
            .acquire_message_content_fetch_outcome(&id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("released claim must be available to the next reader")
            }
        };
        reacquired.release().await.unwrap();
    }

    #[tokio::test]
    async fn dropped_message_content_fetch_owner_releases_its_claim() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Cancelled claim", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();

        let claim = match store
            .acquire_message_content_fetch_outcome(&id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("first reader must own the claim")
            }
        };
        assert!(matches!(
            store
                .acquire_message_content_fetch_outcome(&id)
                .await
                .unwrap(),
            MessageContentFetchAcquire::Busy
        ));
        drop(claim);

        let mut reacquired = false;
        for _ in 0..20 {
            tokio::task::yield_now().await;
            if let MessageContentFetchAcquire::Claimed(claim) = store
                .acquire_message_content_fetch_outcome(&id)
                .await
                .unwrap()
            {
                claim.release().await.unwrap();
                reacquired = true;
                break;
            }
        }
        assert!(reacquired, "drop must release the transient fetch claim");
    }

    #[tokio::test]
    async fn moving_a_message_revokes_the_old_locator_fetch_claim() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut source = message("Moving claim", "preview");
        source.id = stable_message_id(account_id, "INBOX", 1);
        source.account_id = account_id.to_string();
        source.uid = 1;
        let source_id = source.id.clone();
        store.upsert_messages(&[source]).await.unwrap();
        let source_claim = match store
            .acquire_message_content_fetch_outcome(&source_id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("source reader must own the claim")
            }
        };

        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(3))
            .await
            .unwrap();

        let destination_id = stable_message_id(account_id, "Archive", 3);
        let destination_claim = match store
            .acquire_message_content_fetch_outcome(&destination_id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("move must not strand the old fetch claim on the destination")
            }
        };
        source_claim.release().await.unwrap();
        destination_claim.release().await.unwrap();
    }

    #[tokio::test]
    async fn moving_a_starred_message_discards_stale_attachment_ids_and_refetches_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dakia.db");
        let store = Store::open(&path).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut account = AccountDraft {
            email: "moved-star@example.test".into(),
            display_name: "Moved star".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        account.id = account_id;
        store.save_account(&account).await.unwrap();

        let source_id = stable_message_id(account_id, "INBOX", 1);
        let mut source = cache_candidate_message(account_id, &source_id, 1, "INBOX", Utc::now());
        source.is_flagged = true;
        store.upsert_messages(&[source]).await.unwrap();
        let stale_attachment_id = format!("{source_id}:mime-v1:1");
        assert!(store
            .cache_starred_message_content(
                &source_id,
                CachedMessageContent {
                    body_text: "cached starred body".into(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: vec![Attachment {
                        id: stale_attachment_id.clone(),
                        message_id: source_id.clone(),
                        filename: "claim.pdf".into(),
                        mime_type: "application/pdf".into(),
                        size_bytes: 3,
                        is_inline: false,
                        presentation: AttachmentPresentation::Downloadable,
                        is_potentially_unsafe: false,
                    }],
                },
            )
            .await
            .unwrap());

        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(3))
            .await
            .unwrap();
        let destination_id = stable_message_id(account_id, "Archive", 3);
        assert!(store.starred_body(&destination_id).await.unwrap().is_none());
        assert!(store
            .starred_attachment_metadata(&destination_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .uncached_starred_messages(account_id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![destination_id.clone()]
        );

        drop(store);
        let reopened = Store::open(&path).await.unwrap();
        assert!(reopened
            .starred_body(&destination_id)
            .await
            .unwrap()
            .is_none());
        assert!(reopened
            .starred_attachment_metadata(&destination_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened
                .uncached_starred_messages(account_id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![destination_id]
        );
    }

    #[tokio::test]
    async fn message_content_fetch_claims_cascade_and_preserve_fresh_cross_process_leases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dakia.db");
        let store = Store::open(&path).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let first = cache_candidate_message(account_id, "claimed-message", 1, "INBOX", Utc::now());
        let second = cache_candidate_message(account_id, "account-claimed", 2, "INBOX", Utc::now());
        store
            .upsert_messages(&[first.clone(), second.clone()])
            .await
            .unwrap();
        assert!(store.claim_message_content_fetch(&first.id).await.unwrap());
        assert!(store.claim_message_content_fetch(&second.id).await.unwrap());
        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(&first.id)
            .execute(&store.pool)
            .await
            .unwrap();
        let first_claims: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_content_fetches WHERE message_id = ?")
                .bind(&first.id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(first_claims, 0);
        store.delete_account(account_id).await.unwrap();
        let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_content_fetches")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(claims, 0);
        let fresh_account_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, '{}', ?)")
            .bind(fresh_account_id.to_string())
            .bind("restart-claim@example.test")
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        let fresh =
            cache_candidate_message(fresh_account_id, "restart-claimed", 3, "INBOX", Utc::now());
        store
            .upsert_messages(std::slice::from_ref(&fresh))
            .await
            .unwrap();
        assert!(store.claim_message_content_fetch(&fresh.id).await.unwrap());
        sqlx::query("UPDATE message_content_fetches SET claim_owner = 'other-live-process' WHERE message_id = ?")
            .bind(&fresh.id)
            .execute(&store.pool)
            .await
            .unwrap();
        drop(store);

        let store = Store::open(&path).await.unwrap();
        assert!(
            !store.claim_message_content_fetch(&fresh.id).await.unwrap(),
            "opening another process must preserve a fresh foreign claim"
        );
        store
            .release_message_content_fetch(&fresh.id)
            .await
            .unwrap();
        assert!(
            !store.claim_message_content_fetch(&fresh.id).await.unwrap(),
            "a guard from another owner must not release the foreign claim"
        );
        sqlx::query("UPDATE message_content_fetches SET claimed_at = ? WHERE message_id = ?")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS + 1))
            .bind(&fresh.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store.claim_message_content_fetch(&fresh.id).await.unwrap(),
            "a claim older than the bounded lease must be replaceable"
        );
        store
            .release_message_content_fetch(&fresh.id)
            .await
            .unwrap();
        assert!(store.claim_message_content_fetch(&fresh.id).await.unwrap());
    }

    #[tokio::test]
    async fn expired_fetch_takeover_cannot_be_released_by_the_old_guard() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Lease owner", "preview");
        let message_id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();

        let first = store
            .acquire_message_content_fetch(&message_id)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE message_content_fetches SET claimed_at = ? WHERE message_id = ?")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS + 1))
            .bind(&message_id)
            .execute(&store.pool)
            .await
            .unwrap();
        let replacement = store
            .acquire_message_content_fetch(&message_id)
            .await
            .unwrap()
            .expect("an expired claim should be replaceable");

        first.release().await.unwrap();
        assert!(
            store
                .acquire_message_content_fetch(&message_id)
                .await
                .unwrap()
                .is_none(),
            "an old guard must not release a newer owner's claim"
        );
        replacement.release().await.unwrap();
        assert!(store
            .acquire_message_content_fetch(&message_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn body_cache_oversized_replacement_bypasses_without_stale_content() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Oversized", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();
        store
            .cache_message_content(&id, false, cached_content("old"))
            .await
            .unwrap();
        let oversized = "x".repeat(21);
        store
            .cache_message_content_with_budget(&id, false, cached_content(&oversized), 20)
            .await
            .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn body_cache_cascades_through_moves_reconciliation_resets_and_uidvalidity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("dakia.db"))
            .await
            .unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut source = message("Source", "preview");
        source.id = stable_message_id(account_id, "INBOX", 1);
        source.account_id = account_id.to_string();
        let source_id = source.id.clone();
        let mut destination = source.clone();
        destination.id = stable_message_id(account_id, "Archive", 3);
        destination.mailbox = "Archive".into();
        destination.uid = 3;
        let destination_id = destination.id.clone();
        store.upsert_messages(&[source, destination]).await.unwrap();
        let mut source_content = cached_content("source");
        source_content.attachments.push(Attachment {
            id: format!("{source_id}:attachment:0"),
            message_id: source_id.clone(),
            filename: "invoice.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 42,
            is_inline: false,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: false,
        });
        store
            .cache_message_content(&source_id, false, source_content)
            .await
            .unwrap();
        store
            .cache_message_content(&destination_id, false, cached_content("destination"))
            .await
            .unwrap();
        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(3))
            .await
            .unwrap();
        assert!(store
            .cached_message_content(&source_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&destination_id)
            .await
            .unwrap()
            .is_none());
        store
            .move_message(account_id, "Archive", 3, "Trash", None)
            .await
            .unwrap();
        assert!(store
            .cached_message_content(&destination_id)
            .await
            .unwrap()
            .is_none());

        let mut stale = message("Stale", "preview");
        stale.id = stable_message_id(account_id, "INBOX", 10);
        stale.account_id = account_id.to_string();
        stale.uid = 10;
        let stale_id = stale.id.clone();
        let mut retained = stale.clone();
        retained.id = stable_message_id(account_id, "INBOX", 11);
        retained.uid = 11;
        let retained_id = retained.id.clone();
        let mut other_mailbox = retained.clone();
        other_mailbox.id = stable_message_id(account_id, "Archive", 12);
        other_mailbox.mailbox = "Archive".into();
        other_mailbox.uid = 12;
        let other_mailbox_id = other_mailbox.id.clone();
        store
            .upsert_messages(&[stale, retained, other_mailbox])
            .await
            .unwrap();
        for (id, body) in [
            (&stale_id, "stale"),
            (&retained_id, "retained"),
            (&other_mailbox_id, "other"),
        ] {
            store
                .cache_message_content(id, false, cached_content(body))
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .reconcile_mailbox_uids(account_id, "INBOX", &[11].into_iter().collect())
                .await
                .unwrap(),
            1
        );
        assert!(store
            .cached_message_content(&stale_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&retained_id)
            .await
            .unwrap()
            .is_some());
        store
            .reset_mailbox_catalog(account_id, "INBOX")
            .await
            .unwrap();
        assert!(store
            .cached_message_content(&retained_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&other_mailbox_id)
            .await
            .unwrap()
            .is_some());

        let mut regenerated = message("Regenerated", "preview");
        regenerated.id = stable_message_id(account_id, "INBOX", 20);
        regenerated.account_id = account_id.to_string();
        regenerated.uid = 20;
        let regenerated_id = regenerated.id.clone();
        store
            .save_synced_messages(account_id, "INBOX", &[regenerated])
            .await
            .unwrap();
        store
            .cache_message_content(&regenerated_id, false, cached_content("generation"))
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account_id, "INBOX", Some(1))
            .await
            .unwrap();
        store
            .prepare_mailbox_sync(account_id, "INBOX", Some(2))
            .await
            .unwrap();
        assert!(store
            .cached_message_content(&regenerated_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&other_mailbox_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn body_cache_survives_restart_and_rejects_flagged_or_corrupt_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dakia.db");
        let store = Store::open(&path).await.unwrap();
        let message = message("Restart", "preview");
        let id = message.id.clone();
        let account_id = uuid::Uuid::parse_str(&message.account_id).unwrap();
        let mailbox = message.mailbox.clone();
        let uid = u32::try_from(message.uid).unwrap();
        let mut account = AccountDraft {
            email: "body-cache-restart@dakia.dev".into(),
            display_name: "Body cache restart".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        account.id = account_id;
        store.save_account(&account).await.unwrap();
        store.upsert_messages(&[message]).await.unwrap();
        store
            .cache_message_content(&id, false, cached_content("persisted"))
            .await
            .unwrap();
        drop(store);

        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            store
                .cached_message_content(&id)
                .await
                .unwrap()
                .unwrap()
                .body_text,
            "persisted"
        );
        store.set_message_flagged(&id, true).await.unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        store
            .cache_message_content(&id, true, cached_content("must not cache"))
            .await
            .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        store.set_message_flagged(&id, false).await.unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        store
            .cache_message_content(&id, false, cached_content("valid"))
            .await
            .unwrap();
        store
            .update_mailbox_flags(account_id, &mailbox, &[(uid, false, true)])
            .await
            .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        store
            .update_mailbox_flags(account_id, &mailbox, &[(uid, false, false)])
            .await
            .unwrap();
        store
            .cache_message_content(&id, false, cached_content("valid"))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE message_content_cache SET attachments_json = 'not-json' WHERE message_id = ?",
        )
        .bind(&id)
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM message_content_cache WHERE message_id = ?",
            )
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            0,
            "a retry must fetch from the provider instead of repeating the corrupt cache error"
        );
    }

    #[tokio::test]
    async fn searches_across_indexed_messages() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_messages(&[message(
                "Quarterly field notes",
                "The Tallinn launch is Tuesday",
            )])
            .await
            .unwrap();
        let results = store
            .search(&SearchQuery {
                text: "Tallinn".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn local_search_evaluates_parser_terms_before_conversation_paging() {
        let store = Store::in_memory().await.unwrap();
        let mut matching = message("Quarterly invoice", "Body preview");
        matching.id = "search-matching".into();
        matching.mailbox = "Projects/2026".into();
        matching.cc_addresses = "Helen Example <helen@example.com>".into();
        matching.received_at = "2026-09-05T12:00:00Z".parse().unwrap();
        matching.is_read = true;
        matching.is_flagged = true;
        matching.is_answered = true;
        matching.is_draft = true;
        matching.has_attachments = true;
        let mut non_matching = matching.clone();
        non_matching.id = "search-non-matching".into();
        non_matching.uid = 2;
        non_matching.subject = "Quarterly update".into();
        store
            .upsert_messages(&[matching.clone(), non_matching])
            .await
            .unwrap();
        let reloaded = store.message(&matching.id).await.unwrap().unwrap();
        assert!(reloaded.is_answered);
        assert!(reloaded.is_draft);
        sqlx::query("INSERT INTO message_attachment_catalogue(message_id, filename, mime_type, size_bytes, is_inline, presentation) VALUES (?, 'invoice.pdf', 'application/pdf', 1, 0, 'downloadable')")
            .bind(&matching.id)
            .execute(&store.pool)
            .await
            .unwrap();

        let query = SearchQuery {
            text: "subject:\"quarterly invoice\" in:Projects/* cc:helen after:2026-09-01 has:attachment filename:invoice* filetype:pdf is:read is:flagged is:answered is:draft".into(),
            ..Default::default()
        };
        assert_eq!(store.search(&query).await.unwrap()[0].id, matching.id);
        assert_eq!(
            store.search_conversations(&query).await.unwrap()[0]
                .latest
                .id,
            matching.id
        );

        let boolean = SearchQuery {
            text: "subject:missing OR (subject:invoice AND NOT cc:absent)".into(),
            ..Default::default()
        };
        assert_eq!(store.search(&boolean).await.unwrap().len(), 1);
        // `has:noattachment` is only a partial SQLite candidate while MIME
        // metadata is being catalogued. Its negation and the surrounding OR
        // must therefore broaden to canonical evaluation instead of dropping
        // the second row, which has only the durable attachment presence bit.
        let fallback = SearchQuery {
            text: "NOT has:noattachment OR subject:missing".into(),
            ..Default::default()
        };
        assert_eq!(store.search(&fallback).await.unwrap().len(), 2);
        let malformed = SearchQuery {
            text: "subject:\"unterminated".into(),
            ..Default::default()
        };
        assert!(store
            .search(&malformed)
            .await
            .unwrap_err()
            .is::<crate::search::SearchParseError>());
    }

    #[tokio::test]
    async fn conversation_search_evidence_uses_actual_older_body_and_attachment_matches() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let mut older = message("Original", "");
        older.id = "evidence-older".into();
        older.account_id = account_id.to_string();
        older.message_id = Some("<evidence-older@example.test>".into());
        older.received_at = now - chrono::Duration::minutes(2);
        older.content_state = "headers_only".into();

        let mut attachment_match = message("Re: Original", "No matching body");
        attachment_match.id = "evidence-attachment".into();
        attachment_match.account_id = account_id.to_string();
        attachment_match.uid = 2;
        attachment_match.message_id = Some("<evidence-attachment@example.test>".into());
        attachment_match.in_reply_to = older.message_id.clone();
        attachment_match.received_at = now - chrono::Duration::minutes(1);
        attachment_match.attachments.push(AttachmentData {
            attachment: Attachment {
                id: "evidence-attachment:0".into(),
                message_id: attachment_match.id.clone(),
                filename: "report.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 7,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            },
            bytes: Vec::new(),
        });

        let mut newest = message("Re: Original", "Newest non-match");
        newest.id = "evidence-newest".into();
        newest.account_id = account_id.to_string();
        newest.uid = 3;
        newest.message_id = Some("<evidence-newest@example.test>".into());
        newest.in_reply_to = attachment_match.message_id.clone();
        newest.received_at = now;
        store
            .upsert_messages(&[older.clone(), attachment_match.clone(), newest.clone()])
            .await
            .unwrap();
        store
            .cache_search_body_text(&older.id, "Secret older body phrase")
            .await
            .unwrap();

        let body_page = store
            .search_conversation_page(&SearchQuery {
                text: "body:\"older body phrase\"".into(),
                account_ids: vec![account_id],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(body_page.conversations.len(), 1);
        assert_eq!(body_page.conversations[0].latest.id, newest.id);
        let body_evidence = body_page
            .match_evidence
            .get(&body_page.conversations[0].id)
            .unwrap();
        assert_eq!(
            body_evidence.primary_message_id.as_deref(),
            Some(older.id.as_str())
        );
        assert_eq!(body_evidence.matched_message_ids, vec![older.id.clone()]);
        assert_eq!(body_evidence.match_count, 1);
        assert_eq!(
            body_evidence.excerpt.as_deref(),
            Some("Secret older body phrase")
        );

        for query in ["filename:report*", "filetype:pdf"] {
            let page = store
                .search_conversation_page(&SearchQuery {
                    text: query.into(),
                    account_ids: vec![account_id],
                    ..Default::default()
                })
                .await
                .unwrap();
            let evidence = page.match_evidence.get(&page.conversations[0].id).unwrap();
            assert_eq!(
                evidence.primary_message_id.as_deref(),
                Some(attachment_match.id.as_str()),
                "{query} evidence must identify the actual matching member"
            );
        }
    }

    #[test]
    fn search_query_defaults_unflagged_filter_for_existing_ipc_callers() {
        let query: SearchQuery = serde_json::from_value(serde_json::json!({
            "text": "receipt",
            "account_ids": [],
            "unread_only": true,
            "flagged_only": false
        }))
        .unwrap();
        assert!(!query.unflagged_only);
    }

    #[tokio::test]
    async fn search_pages_are_stable_and_do_not_overlap() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let messages = (0..125)
            .map(|index| {
                let mut item = message(&format!("Message {index}"), "Body");
                item.id = format!("message-{index:03}");
                item.thread_id = item.id.clone();
                item.account_id = account_id.clone();
                item.uid = index + 1;
                item.received_at = now - chrono::Duration::minutes(index);
                item
            })
            .collect::<Vec<_>>();
        store.upsert_messages(&messages).await.unwrap();

        let first = store
            .search(&SearchQuery {
                account_ids: vec![account_id.parse().unwrap()],
                limit: Some(100),
                ..Default::default()
            })
            .await
            .unwrap();
        let second = store
            .search(&SearchQuery {
                account_ids: vec![account_id.parse().unwrap()],
                limit: Some(100),
                cursor: Some(MailCursor {
                    received_at: first.last().unwrap().received_at,
                    id: first.last().unwrap().id.clone(),
                }),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(first.len(), 100);
        assert_eq!(second.len(), 25);
        assert!(first
            .iter()
            .all(|item| second.iter().all(|next| item.id != next.id)));
        assert!(first.last().unwrap().received_at > second[0].received_at);
    }

    #[tokio::test]
    async fn unflagged_unread_category_pages_are_stable_across_accounts_and_ties() {
        let store = Store::in_memory().await.unwrap();
        let first_account = uuid::Uuid::new_v4();
        let second_account = uuid::Uuid::new_v4();
        let excluded_account = uuid::Uuid::new_v4();
        let timestamp = Utc::now();
        let mut messages = Vec::new();
        for index in 0..43 {
            let account = if index % 2 == 0 {
                first_account
            } else {
                second_account
            };
            let mut item = message(&format!("Eligible {index}"), "Body");
            item.id = format!("eligible-{index:03}");
            item.account_id = account.to_string();
            item.uid = (index + 1) as i64;
            item.message_id = Some(if index < 2 {
                "<shared-across-accounts@example.com>".into()
            } else {
                format!("<eligible-{index}@example.com>")
            });
            item.received_at = timestamp;
            item.category = Some("people".into());
            messages.push(item);
        }

        // The category view must suppress the root as well as this older,
        // flagged reply. Keeping it older verifies this is not merely a
        // representative-row `is_flagged = 0` predicate.
        let mut flagged_member = messages[10].clone();
        flagged_member.id = "flagged-thread-member".into();
        flagged_member.uid = 900;
        flagged_member.message_id = Some("<flagged-thread-member@example.com>".into());
        flagged_member.in_reply_to = messages[10].message_id.clone();
        flagged_member.reference_ids = messages[10].message_id.clone();
        flagged_member.received_at = timestamp - chrono::Duration::seconds(1);
        flagged_member.is_flagged = true;
        messages.push(flagged_member);

        let mut read_decoy = message("Read decoy", "Body");
        read_decoy.id = "read-decoy".into();
        read_decoy.account_id = first_account.to_string();
        read_decoy.uid = 901;
        read_decoy.received_at = timestamp;
        read_decoy.category = Some("people".into());
        read_decoy.is_read = true;
        messages.push(read_decoy);

        let mut category_decoy = message("Other category", "Body");
        category_decoy.id = "category-decoy".into();
        category_decoy.account_id = second_account.to_string();
        category_decoy.uid = 902;
        category_decoy.received_at = timestamp;
        category_decoy.category = Some("other".into());
        messages.push(category_decoy);

        let mut account_decoy = message("Other account", "Body");
        account_decoy.id = "account-decoy".into();
        account_decoy.account_id = excluded_account.to_string();
        account_decoy.uid = 1;
        account_decoy.received_at = timestamp;
        account_decoy.category = Some("people".into());
        messages.push(account_decoy);
        store.upsert_messages(&messages).await.unwrap();

        let query = SearchQuery {
            account_ids: vec![first_account, second_account],
            mailbox: Some("INBOX".into()),
            unread_only: true,
            unflagged_only: true,
            category: Some("people".into()),
            limit: Some(3),
            ..Default::default()
        };
        let first = store.search_conversation_page(&query).await.unwrap();
        assert_eq!(first.conversations.len(), 3);
        let mut cursor = first.next_cursor.clone().expect("more than three matches");
        let mut seen = first
            .conversations
            .iter()
            .map(|conversation| conversation.id.clone())
            .collect::<Vec<_>>();
        let mut page_sizes = vec![first.conversations.len()];
        let exhausted_cursor;

        loop {
            let page = store
                .search_conversation_page(&SearchQuery {
                    limit: Some(20),
                    cursor: Some(cursor.clone()),
                    ..query.clone()
                })
                .await
                .unwrap();
            page_sizes.push(page.conversations.len());
            seen.extend(
                page.conversations
                    .iter()
                    .map(|conversation| conversation.id.clone()),
            );
            match page.next_cursor {
                Some(next) => cursor = next,
                None => {
                    let last = page
                        .conversations
                        .last()
                        .expect("the final page still has matches");
                    exhausted_cursor = MailCursor {
                        received_at: last.latest.received_at,
                        id: last.latest.id.clone(),
                    };
                    break;
                }
            }
        }

        assert_eq!(page_sizes, [3, 20, 19]);
        assert_eq!(seen.len(), 42);
        assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 42);
        assert!(seen.iter().all(|id| !id.contains("eligible-010")));
        assert_eq!(
            seen.iter()
                .filter(|id| id.ends_with(":shared-across-accounts@example.com"))
                .count(),
            2
        );
        assert!(store
            .search_conversation_page(&SearchQuery {
                limit: Some(20),
                cursor: Some(exhausted_cursor),
                ..query.clone()
            })
            .await
            .unwrap()
            .conversations
            .is_empty());

        let raw = store
            .search(&SearchQuery {
                limit: Some(100),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(raw.len(), 42);
        assert!(raw
            .iter()
            .all(|message| !message.is_read && !message.is_flagged));
    }

    #[tokio::test]
    async fn category_uses_latest_thread_member_while_unread_checks_the_scoped_thread() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let timestamp = Utc::now();
        let mut older = message("Category root", "Body");
        older.id = "category-root".into();
        older.account_id = account_id.to_string();
        older.uid = 1;
        older.message_id = Some("<category-root@example.com>".into());
        older.received_at = timestamp - chrono::Duration::minutes(1);
        older.category = Some("people".into());
        older.is_read = false;
        let mut newer = message("Re: Category root", "Body");
        newer.id = "category-newer".into();
        newer.account_id = account_id.to_string();
        newer.uid = 2;
        newer.message_id = Some("<category-newer@example.com>".into());
        newer.in_reply_to = older.message_id.clone();
        newer.reference_ids = older.message_id.clone();
        newer.received_at = timestamp;
        newer.category = Some("transactions".into());
        newer.is_read = true;
        store.upsert_messages(&[older, newer]).await.unwrap();

        let transactions = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                category: Some("transactions".into()),
                unread_only: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(transactions.conversations.len(), 1);
        assert_eq!(transactions.conversations[0].latest.id, "category-newer");
        assert!(transactions.conversations[0].unread);
        assert!(store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                category: Some("people".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .conversations
            .is_empty());
    }

    #[tokio::test]
    async fn smart_inbox_batches_all_initial_sections_with_existing_membership_semantics() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let timestamp = Utc::now();
        let mut messages = Vec::new();
        for (id, category, is_read, is_flagged, offset) in [
            ("people-new", "people", false, false, 0),
            ("people-old", "people", false, false, 1),
            ("starred", "people", false, true, 2),
            ("newsletter", "newsletters", false, false, 3),
            ("seen", "notifications", true, false, 4),
        ] {
            let mut item = message(id, "Body");
            item.id = id.into();
            item.thread_id = id.into();
            item.message_id = Some(format!("<{id}@example.com>"));
            item.account_id = account_id.to_string();
            item.uid = offset + 1;
            item.received_at = timestamp - chrono::Duration::minutes(offset);
            item.category = Some(category.into());
            item.is_read = is_read;
            item.is_flagged = is_flagged;
            messages.push(item);
        }
        store.upsert_messages(&messages).await.unwrap();

        let page = store
            .search_smart_inbox(&SmartInboxQuery {
                account_ids: vec![account_id],
                limit: Some(1),
            })
            .await
            .unwrap();
        assert_eq!(
            page.sections
                .iter()
                .map(|section| section.id.as_str())
                .collect::<Vec<_>>(),
            [
                "starred",
                "people",
                "transactions",
                "notifications",
                "newsletters",
                "other",
                "seen",
            ]
        );
        let sections = page
            .sections
            .iter()
            .map(|section| (section.id.as_str(), section))
            .collect::<HashMap<_, _>>();
        assert_eq!(sections["starred"].conversations[0].latest.id, "starred");
        assert_eq!(sections["people"].conversations[0].latest.id, "people-new");
        assert!(sections["people"].next_cursor.is_some());
        assert_eq!(
            sections["newsletters"].conversations[0].latest.id,
            "newsletter"
        );
        assert_eq!(sections["seen"].conversations[0].latest.id, "seen");
        assert!(sections["transactions"].conversations.is_empty());
        assert!(sections["notifications"].conversations.is_empty());
        assert!(sections["other"].conversations.is_empty());
        assert!(sections["starred"]
            .conversations
            .iter()
            .all(|conversation| conversation.latest.id != "people-new"));
    }

    #[tokio::test]
    async fn smart_category_pagination_has_no_cursor_at_the_exact_third_page_boundary() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let timestamp = Utc::now();
        let messages = (0..43)
            .map(|index| {
                let mut item = message(&format!("Smart {index}"), "Body");
                item.id = format!("smart-{index:03}");
                item.account_id = account_id.to_string();
                item.uid = index + 1;
                item.message_id = Some(format!("<smart-{index}@example.com>"));
                item.received_at = timestamp;
                item.category = Some("people".into());
                item
            })
            .collect::<Vec<_>>();
        store.upsert_messages(&messages).await.unwrap();

        let query = SearchQuery {
            account_ids: vec![account_id],
            mailbox: Some("INBOX".into()),
            unread_only: true,
            unflagged_only: true,
            category: Some("people".into()),
            limit: Some(3),
            ..Default::default()
        };
        let first = store.search_conversation_page(&query).await.unwrap();
        let second = store
            .search_conversation_page(&SearchQuery {
                limit: Some(20),
                cursor: first.next_cursor.clone(),
                ..query.clone()
            })
            .await
            .unwrap();
        let third = store
            .search_conversation_page(&SearchQuery {
                limit: Some(20),
                cursor: second.next_cursor.clone(),
                ..query
            })
            .await
            .unwrap();

        assert_eq!(first.conversations.len(), 3);
        assert!(first.next_cursor.is_some());
        assert_eq!(second.conversations.len(), 20);
        assert!(second.next_cursor.is_some());
        assert_eq!(third.conversations.len(), 20);
        assert!(third.next_cursor.is_none());
        let seen = first
            .conversations
            .iter()
            .chain(&second.conversations)
            .chain(&third.conversations)
            .map(|conversation| conversation.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(seen.len(), 43);
    }

    #[tokio::test]
    async fn starred_conversations_order_by_latest_scoped_member_not_latest_flagged_member() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut root = message("Flagged root", "Body");
        root.id = "starred-root".into();
        root.account_id = account_id.to_string();
        root.uid = 1;
        root.message_id = Some("<starred-root@example.com>".into());
        root.received_at = "2026-01-01T00:00:00Z".parse().unwrap();
        root.is_flagged = true;

        let mut reply = message("Re: Flagged root", "Body");
        reply.id = "starred-newest-reply".into();
        reply.account_id = account_id.to_string();
        reply.uid = 2;
        reply.message_id = Some("<starred-newest-reply@example.com>".into());
        reply.in_reply_to = root.message_id.clone();
        reply.reference_ids = root.message_id.clone();
        reply.received_at = "2026-01-10T00:00:00Z".parse().unwrap();

        let mut january_nine = message("January nine", "Body");
        january_nine.id = "starred-january-nine".into();
        january_nine.account_id = account_id.to_string();
        january_nine.uid = 3;
        january_nine.message_id = Some("<starred-january-nine@example.com>".into());
        january_nine.received_at = "2026-01-09T00:00:00Z".parse().unwrap();
        january_nine.is_flagged = true;

        let mut january_seven = message("January seven", "Body");
        january_seven.id = "starred-january-seven".into();
        january_seven.account_id = account_id.to_string();
        january_seven.uid = 4;
        january_seven.message_id = Some("<starred-january-seven@example.com>".into());
        january_seven.received_at = "2026-01-07T00:00:00Z".parse().unwrap();
        january_seven.is_flagged = true;
        store
            .upsert_messages(&[root, reply, january_nine, january_seven])
            .await
            .unwrap();

        let query = SearchQuery {
            account_ids: vec![account_id],
            mailbox: Some("INBOX".into()),
            flagged_only: true,
            limit: Some(2),
            ..Default::default()
        };
        let first = store.search_conversation_page(&query).await.unwrap();
        assert_eq!(
            first
                .conversations
                .iter()
                .map(|conversation| conversation.latest.id.as_str())
                .collect::<Vec<_>>(),
            ["starred-newest-reply", "starred-january-nine"]
        );
        let second = store
            .search_conversation_page(&SearchQuery {
                cursor: first.next_cursor.clone(),
                ..query
            })
            .await
            .unwrap();
        assert_eq!(
            second
                .conversations
                .iter()
                .map(|conversation| conversation.latest.id.as_str())
                .collect::<Vec<_>>(),
            ["starred-january-seven"]
        );
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn conversation_pages_are_keyset_stable_for_ties_threads_and_changes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let timestamp = Utc::now();
        let messages = (0..6)
            .map(|index| {
                let mut item = message(&format!("Message {index}"), "Body");
                item.id = format!("message-{index}");
                item.thread_id = format!("thread-{index}");
                item.account_id = account_id.to_string();
                item.uid = index + 1;
                item.received_at = timestamp;
                item
            })
            .collect::<Vec<_>>();
        store.upsert_messages(&messages).await.unwrap();

        let query = SearchQuery {
            account_ids: vec![account_id],
            limit: Some(2),
            ..Default::default()
        };
        let first = store.search_conversation_page(&query).await.unwrap();
        assert_eq!(
            first
                .conversations
                .iter()
                .map(|conversation| conversation.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["message-5", "message-4"]
        );
        let cursor = first.next_cursor.clone().expect("third thread remains");

        // A newly inserted message sorts before the cursor and must not shift
        // the continuation. Removing an unseen row must not create a gap.
        let mut newer = message("Newer", "Body");
        newer.id = "message-newer".into();
        newer.thread_id = "thread-newer".into();
        newer.account_id = account_id.to_string();
        newer.uid = 99;
        newer.received_at = timestamp + chrono::Duration::seconds(1);
        store.upsert_messages(&[newer]).await.unwrap();
        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind("message-3")
            .execute(&store.pool)
            .await
            .unwrap();

        let second = store
            .search_conversation_page(&SearchQuery {
                cursor: Some(cursor),
                ..query.clone()
            })
            .await
            .unwrap();
        let third = store
            .search_conversation_page(&SearchQuery {
                cursor: second.next_cursor.clone(),
                ..query
            })
            .await
            .unwrap();
        let seen = first
            .conversations
            .iter()
            .chain(&second.conversations)
            .chain(&third.conversations)
            .map(|conversation| conversation.thread_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            [
                "message-5",
                "message-4",
                "message-2",
                "message-1",
                "message-0"
            ]
        );
        assert!(seen.iter().all(|thread_id| *thread_id != "thread-newer"));
        assert!(third.next_cursor.is_none());
    }

    #[tokio::test]
    async fn conversation_page_uses_one_match_per_thread_and_exact_has_more() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let mut messages = Vec::new();
        for index in 0..5 {
            let mut item = message(&format!("Match {index}"), "Body");
            item.id = format!("thread-message-{index}");
            item.account_id = account_id.to_string();
            item.uid = index + 1;
            item.received_at = now - chrono::Duration::minutes(index);
            item.message_id = Some(format!("<thread-message-{index}@example.com>"));
            if matches!(index, 1 | 2) {
                item.in_reply_to = Some("<thread-message-0@example.com>".into());
                item.reference_ids = Some("<thread-message-0@example.com>".into());
            }
            messages.push(item);
        }
        store.upsert_messages(&messages).await.unwrap();

        let first = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(first.conversations.len(), 2);
        assert_eq!(first.conversations[0].message_count, 3);
        let second = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                limit: Some(2),
                cursor: first.next_cursor.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(second.conversations.len(), 1);
        assert_eq!(second.conversations[0].latest.id, "thread-message-4");
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn conversation_page_normalizes_zero_and_keeps_the_501st_lookahead() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let messages = (0..501)
            .map(|index| {
                let mut item = message(&format!("Boundary {index}"), "Body");
                item.id = format!("boundary-{index:03}");
                item.account_id = account_id.to_string();
                item.uid = index + 1;
                item.received_at = now - chrono::Duration::seconds(index);
                item
            })
            .collect::<Vec<_>>();
        store.upsert_messages(&messages).await.unwrap();

        let zero = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                limit: Some(0),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(zero.conversations.len(), 1);
        assert!(zero.next_cursor.is_some());

        let query = SearchQuery {
            account_ids: vec![account_id],
            limit: Some(500),
            ..Default::default()
        };
        // A 500-row page no longer turns a broad local search into a 501-row
        // canonical evaluation. The first turn stops at the fixed candidate
        // budget and returns its opaque V2 continuation state instead.
        let first = store
            .search_conversation_page_from_candidate(&query, None, &[])
            .await
            .unwrap();
        assert_eq!(first.conversations.len(), SEARCH_CANDIDATE_SCAN_BUDGET);
        assert!(first.next_cursor.is_none());
        assert!(first.candidate_cursor.is_some());
        assert!(!first.candidate_exhausted);

        let emitted = first
            .conversations
            .iter()
            .map(|conversation| conversation.id.clone())
            .collect::<Vec<_>>();
        let remainder = store
            .search_conversation_page_from_candidate(
                &query,
                first.candidate_cursor.as_ref(),
                &emitted,
            )
            .await
            .unwrap();
        assert_eq!(
            remainder.conversations.len(),
            501 - SEARCH_CANDIDATE_SCAN_BUDGET
        );
        assert!(remainder.candidate_cursor.is_none());
        assert!(remainder.candidate_exhausted);
        let all_ids = first
            .conversations
            .iter()
            .chain(remainder.conversations.iter())
            .map(|conversation| conversation.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(
            all_ids.len(),
            501,
            "continuation must not skip or repeat a thread"
        );
    }

    #[test]
    fn conversation_page_serializes_the_tauri_continuation_as_next_cursor() {
        let page = MailConversationPage {
            conversations: Vec::new(),
            match_evidence: BTreeMap::new(),
            next_cursor: Some(MailCursor {
                received_at: "2026-07-27T12:00:00Z".parse().unwrap(),
                id: "message-id".into(),
            }),
            candidate_cursor: None,
            candidate_exhausted: true,
        };
        let value = serde_json::to_value(page).unwrap();
        assert_eq!(value["nextCursor"]["id"], "message-id");
    }

    #[tokio::test]
    async fn inbox_conversation_hydrates_sent_and_archive_members() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut root = message("Project update", "Original inbox message");
        root.id = "inbox-root".into();
        root.account_id = account_id.to_string();
        root.message_id = Some("<root@example.com>".into());
        root.thread_id = root.id.clone();
        root.received_at = Utc::now() - chrono::Duration::hours(2);

        let mut sent = message("Re: Project update", "My sent reply");
        sent.id = "sent-reply".into();
        sent.account_id = account_id.to_string();
        sent.mailbox = "Sent::Sent Messages".into();
        sent.uid = 2;
        sent.from_name = Some("Me".into());
        sent.from_address = "me@example.com".into();
        sent.to_addresses = "mara@example.com".into();
        sent.message_id = Some("<reply@example.com>".into());
        sent.in_reply_to = Some("<root@example.com>".into());
        sent.reference_ids = Some("<root@example.com>".into());
        sent.thread_id = sent.id.clone();
        sent.received_at = Utc::now() - chrono::Duration::hours(1);
        store.upsert_messages(&[root, sent.clone()]).await.unwrap();

        let inbox = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].message_count, 2);
        assert_eq!(inbox[0].latest.id, sent.id);
        assert!(inbox[0]
            .messages
            .iter()
            .all(|message| message.body_text.is_empty()
                && message.body_html.is_none()
                && message.classification_signals.is_empty()));
        assert_eq!(
            inbox[0]
                .messages
                .iter()
                .map(|message| message.mailbox.as_str())
                .collect::<Vec<_>>(),
            ["INBOX", "Sent::Sent Messages"]
        );

        let sent_view = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("Sent".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(sent_view.len(), 1);
        assert_eq!(sent_view[0].message_count, 2);

        let search = store
            .search_conversations(&SearchQuery {
                text: "Original inbox".into(),
                account_ids: vec![account_id],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].message_count, 2);
        assert_eq!(
            search[0].latest.id, sent.id,
            "the newest conversation display message remains intact when an older member matches"
        );
        assert!(
            search[0]
                .source_messages
                .iter()
                .any(|message| message.id == "inbox-root"),
            "the matching source message remains part of the hydrated conversation"
        );

        let mut follow_up = message("Re: Project update", "Incoming follow-up");
        follow_up.id = "incoming-follow-up".into();
        follow_up.account_id = account_id.to_string();
        follow_up.uid = 3;
        follow_up.message_id = Some("<follow-up@example.com>".into());
        follow_up.in_reply_to = Some("<reply@example.com>".into());
        follow_up.reference_ids = Some("<root@example.com> <reply@example.com>".into());
        follow_up.received_at = Utc::now();
        store.upsert_messages(&[follow_up]).await.unwrap();
        let updated = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].message_count, 3);
    }

    #[tokio::test]
    async fn seen_conversations_require_every_scoped_message_to_be_read() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();

        let mut seen = message("Seen", "Read inbox message");
        seen.id = "seen-inbox".into();
        seen.account_id = account_id.to_string();
        seen.thread_id = "seen-thread".into();
        seen.is_read = true;

        let mut sent_unread = message("Re: Seen", "Unread sent copy");
        sent_unread.id = "seen-sent".into();
        sent_unread.account_id = account_id.to_string();
        sent_unread.thread_id = seen.thread_id.clone();
        sent_unread.mailbox = "Sent".into();
        sent_unread.uid = 2;
        sent_unread.is_read = false;

        let mut mixed_read = message("Mixed", "Read member");
        mixed_read.id = "mixed-read".into();
        mixed_read.account_id = account_id.to_string();
        mixed_read.thread_id = "mixed-thread".into();
        mixed_read.uid = 3;
        mixed_read.is_read = true;

        let mut mixed_unread = message("Re: Mixed", "Unread member");
        mixed_unread.id = "mixed-unread".into();
        mixed_unread.account_id = account_id.to_string();
        mixed_unread.thread_id = mixed_read.thread_id.clone();
        mixed_unread.uid = 4;
        mixed_unread.is_read = false;

        store
            .upsert_messages(&[seen, sent_unread, mixed_read, mixed_unread])
            .await
            .unwrap();

        let page = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                read_only: true,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(page.conversations.len(), 1);
        assert!(page.conversations[0]
            .messages
            .iter()
            .any(|message| message.id == "seen-inbox"));
        assert!(page.conversations[0]
            .messages
            .iter()
            .all(|message| !message.id.starts_with("mixed-")));
    }

    #[tokio::test]
    async fn archive_removes_inbox_members_but_preserves_sent_history() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("Question", "Question");
        inbox.id = stable_message_id(account_id, "INBOX", 1);
        inbox.account_id = account_id.to_string();
        inbox.message_id = Some("<question@example.com>".into());
        inbox.thread_id = inbox.id.clone();
        let mut sent = message("Re: Question", "Answer");
        sent.id = stable_message_id(account_id, "Sent", 2);
        sent.account_id = account_id.to_string();
        sent.mailbox = "Sent".into();
        sent.uid = 2;
        sent.message_id = Some("<answer@example.com>".into());
        sent.in_reply_to = Some("<question@example.com>".into());
        sent.thread_id = sent.id.clone();
        store.upsert_messages(&[inbox, sent]).await.unwrap();

        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(3))
            .await
            .unwrap();
        let inbox_view = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(inbox_view.is_empty());
        let sent_view = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("Sent".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(sent_view.len(), 1);
        assert_eq!(sent_view[0].message_count, 2);
        assert!(sent_view[0]
            .messages
            .iter()
            .any(|message| message.mailbox == "Sent"));
    }

    #[tokio::test]
    async fn completed_mailbox_action_rejects_a_late_realtime_write() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut stale_realtime_header = message("Archive me", "Stale header");
        stale_realtime_header.id = stable_message_id(account_id, "INBOX", 41);
        stale_realtime_header.account_id = account_id.to_string();
        stale_realtime_header.uid = 41;
        store
            .upsert_messages(std::slice::from_ref(&stale_realtime_header))
            .await
            .unwrap();

        store
            .move_message(account_id, "INBOX", 41, "Archive", None)
            .await
            .unwrap();
        store
            .save_synced_messages(account_id, "INBOX", &[stale_realtime_header.clone()])
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());

        // A UIDVALIDITY reset permits the same numeric UID to represent a new
        // provider message, so mailbox-scoped tombstones must be discarded.
        store
            .reset_mailbox_catalog(account_id, "INBOX")
            .await
            .unwrap();
        store
            .upsert_catalog_messages(&[stale_realtime_header])
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn batch_move_preserves_destination_when_a_later_source_locator_is_absent() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut inbox = message("Newsletter", "Sender cleanup source");
        inbox.id = stable_message_id(account_id, "INBOX", 41);
        inbox.account_id = account_id.to_string();
        inbox.mailbox = "INBOX".into();
        inbox.uid = 41;
        store.upsert_messages(&[inbox.clone()]).await.unwrap();

        store
            .move_messages_to_destination(
                account_id,
                &[("INBOX".into(), 41), ("Archive".into(), 99)],
                "Trash",
                Some(7),
            )
            .await
            .unwrap();

        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());
        let trash = store
            .message_by_locator(account_id, "Trash", 7)
            .await
            .unwrap()
            .expect("the existing source must become the Trash row");
        assert_eq!(trash.subject, inbox.subject);

        let mut stale_archive = inbox.clone();
        stale_archive.id = stable_message_id(account_id, "Archive", 99);
        stale_archive.mailbox = "Archive".into();
        stale_archive.uid = 99;
        store
            .save_synced_messages(account_id, "INBOX", &[inbox])
            .await
            .unwrap();
        store
            .save_synced_messages(account_id, "Archive", &[stale_archive])
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "Archive", 99)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "Trash", 7)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn permanent_delete_tombstones_only_the_exact_locator_and_preserves_other_rows() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;

        let mut deleted = message("Delete me", "target");
        deleted.id = stable_message_id(account_id, "INBOX", 41);
        deleted.account_id = account_id.to_string();
        deleted.uid = 41;
        let deleted_id = deleted.id.clone();

        let mut inbox_sibling = deleted.clone();
        inbox_sibling.id = stable_message_id(account_id, "INBOX", 42);
        inbox_sibling.subject = "Keep inbox sibling".into();
        inbox_sibling.uid = 42;
        let inbox_sibling_id = inbox_sibling.id.clone();

        let mut same_uid_other_mailbox = deleted.clone();
        same_uid_other_mailbox.id = stable_message_id(account_id, "Archive", 41);
        same_uid_other_mailbox.mailbox = "Archive".into();
        same_uid_other_mailbox.subject = "Keep archive copy".into();
        let same_uid_other_mailbox_id = same_uid_other_mailbox.id.clone();

        store
            .upsert_messages(&[
                deleted.clone(),
                inbox_sibling.clone(),
                same_uid_other_mailbox.clone(),
            ])
            .await
            .unwrap();
        for (id, body) in [
            (&deleted_id, "deleted cache"),
            (&inbox_sibling_id, "inbox cache"),
            (&same_uid_other_mailbox_id, "archive cache"),
        ] {
            store
                .cache_message_content(id, false, cached_content(body))
                .await
                .unwrap();
        }

        store
            .move_message(account_id, "INBOX", 41, "", None)
            .await
            .unwrap();

        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "INBOX", 42)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .message_by_locator(account_id, "Archive", 41)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .cached_message_content(&deleted_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&inbox_sibling_id)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .cached_message_content(&same_uid_other_mailbox_id)
            .await
            .unwrap()
            .is_some());

        let tombstones: Vec<(String, i64)> = sqlx::query_as(
            "SELECT mailbox, uid FROM mailbox_action_tombstones WHERE account_id = ? ORDER BY mailbox, uid",
        )
        .bind(account_id.to_string())
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(tombstones, vec![("INBOX".into(), 41)]);

        store
            .save_synced_messages(account_id, "INBOX", &[deleted])
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn recent_catalogue_refresh_preserves_completed_local_flags_on_conflict() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut existing = message("Initial subject", "preview");
        existing.id = stable_message_id(account_id, "INBOX", 12);
        existing.account_id = account_id.to_string();
        existing.uid = 12;
        store
            .upsert_catalog_messages(&[existing.clone()])
            .await
            .unwrap();
        let expected_flags = store
            .capture_recent_catalogue_expected_flags(account_id, "INBOX", &[12])
            .await
            .unwrap();

        // Model a completed user action landing after the periodic refresh
        // read the provider's old (unread, unstarred) flags.
        store
            .update_mailbox_flags(account_id, "INBOX", &[(12, true, true)])
            .await
            .unwrap();
        let mut delayed_existing = existing.clone();
        delayed_existing.subject = "Provider metadata still updates".into();
        delayed_existing.is_read = false;
        delayed_existing.is_flagged = false;
        let mut inserted = message("New provider row", "preview");
        inserted.id = stable_message_id(account_id, "INBOX", 13);
        inserted.account_id = account_id.to_string();
        inserted.uid = 13;
        inserted.is_read = true;
        inserted.is_flagged = true;

        store
            .upsert_recent_catalog_messages(&[delayed_existing, inserted.clone()], &expected_flags)
            .await
            .unwrap();

        let retained = store.message(&existing.id).await.unwrap().unwrap();
        assert_eq!(retained.subject, "Provider metadata still updates");
        assert!(retained.is_read);
        assert!(retained.is_flagged);
        let new_row = store.message(&inserted.id).await.unwrap().unwrap();
        assert!(new_row.is_read);
        assert!(new_row.is_flagged);
    }

    #[tokio::test]
    async fn recent_catalogue_refresh_applies_provider_flags_when_snapshot_is_current() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut existing = message("Initial", "preview");
        existing.id = stable_message_id(account_id, "INBOX", 14);
        existing.account_id = account_id.to_string();
        existing.uid = 14;
        store
            .upsert_catalog_messages(&[existing.clone()])
            .await
            .unwrap();
        let expected_flags = store
            .capture_recent_catalogue_expected_flags(account_id, "INBOX", &[14])
            .await
            .unwrap();
        let mut provider_refresh = existing.clone();
        provider_refresh.is_read = true;
        provider_refresh.is_flagged = true;

        store
            .upsert_recent_catalog_messages(&[provider_refresh], &expected_flags)
            .await
            .unwrap();

        let stored = store.message(&existing.id).await.unwrap().unwrap();
        assert!(stored.is_read);
        assert!(stored.is_flagged);
    }

    #[tokio::test]
    async fn recent_catalogue_refresh_does_not_reverse_local_unread_unstar_actions() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut existing = message("Initially read and starred", "preview");
        existing.id = stable_message_id(account_id, "INBOX", 15);
        existing.account_id = account_id.to_string();
        existing.uid = 15;
        existing.is_read = true;
        existing.is_flagged = true;
        store
            .upsert_catalog_messages(&[existing.clone()])
            .await
            .unwrap();
        let expected_flags = store
            .capture_recent_catalogue_expected_flags(account_id, "INBOX", &[15])
            .await
            .unwrap();
        store
            .update_mailbox_flags(account_id, "INBOX", &[(15, false, false)])
            .await
            .unwrap();
        let mut delayed_provider_flags = existing.clone();
        delayed_provider_flags.is_read = true;
        delayed_provider_flags.is_flagged = true;

        store
            .upsert_recent_catalog_messages(&[delayed_provider_flags], &expected_flags)
            .await
            .unwrap();

        let stored = store.message(&existing.id).await.unwrap().unwrap();
        assert!(!stored.is_read);
        assert!(!stored.is_flagged);
    }

    #[tokio::test]
    async fn all_mail_excludes_system_mailboxes_and_spam_is_isolated() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("Linked", "Inbox");
        inbox.id = "normal".into();
        inbox.account_id = account_id.to_string();
        inbox.message_id = Some("<normal@example.com>".into());
        let mut spam = message("Re: Linked", "Spam copy");
        spam.id = "spam".into();
        spam.account_id = account_id.to_string();
        spam.mailbox = "Spam::Bulk".into();
        spam.uid = 2;
        spam.message_id = Some("<spam@example.com>".into());
        spam.in_reply_to = Some("<normal@example.com>".into());
        let mut trash = message("Deleted", "Trash");
        trash.id = "trash".into();
        trash.account_id = account_id.to_string();
        trash.mailbox = "Trash".into();
        trash.uid = 3;
        store.upsert_messages(&[inbox, spam, trash]).await.unwrap();

        let all = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].message_count, 1);
        let spam_view = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("Spam".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(spam_view.len(), 1);
        assert_eq!(spam_view[0].messages.len(), 1);
        assert_eq!(spam_view[0].messages[0].mailbox, "Spam::Bulk");
    }

    #[tokio::test]
    async fn explicit_folder_search_keeps_spam_and_trash_in_flat_and_conversation_results() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("Inbox", "ordinary");
        inbox.id = "folder-inbox".into();
        inbox.account_id = account_id.to_string();
        inbox.thread_id = inbox.id.clone();
        let mut spam = message("Spam", "selected explicitly");
        spam.id = "folder-spam".into();
        spam.account_id = account_id.to_string();
        spam.thread_id = spam.id.clone();
        spam.mailbox = "Spam::Bulk".into();
        spam.uid = 2;
        let mut trash = message("Trash", "selected explicitly");
        trash.id = "folder-trash".into();
        trash.account_id = account_id.to_string();
        trash.thread_id = trash.id.clone();
        trash.mailbox = "Trash".into();
        trash.uid = 3;
        store
            .upsert_messages(&[inbox, spam.clone(), trash.clone()])
            .await
            .unwrap();

        for (text, expected) in [
            ("in:Spam", vec![spam.id.clone()]),
            ("in:spam", vec![spam.id.clone()]),
            ("in:\"Bulk\"", vec![spam.id.clone()]),
            ("in:Trash", vec![trash.id.clone()]),
            ("in:TRASH", vec![trash.id.clone()]),
            (
                "in:*",
                vec!["folder-inbox".into(), spam.id.clone(), trash.id.clone()],
            ),
            ("NOT in:Sent", vec!["folder-inbox".into()]),
            (
                "in:Inbox OR subject:explicitly",
                vec!["folder-inbox".into()],
            ),
            ("NOT NOT in:Spam", vec![spam.id.clone()]),
            ("", vec!["folder-inbox".into()]),
        ] {
            let query = SearchQuery {
                text: text.into(),
                account_ids: vec![account_id],
                ..Default::default()
            };
            let mut flat = store
                .search(&query)
                .await
                .unwrap()
                .into_iter()
                .map(|message| message.id)
                .collect::<Vec<_>>();
            let mut conversations = store
                .search_conversation_page(&query)
                .await
                .unwrap()
                .conversations
                .into_iter()
                .map(|conversation| conversation.latest.id)
                .collect::<Vec<_>>();
            flat.sort();
            conversations.sort();
            let mut expected = expected;
            expected.sort();
            assert_eq!(flat, expected, "flat {text:?}");
            assert_eq!(conversations, expected, "conversation {text:?}");
        }
    }

    #[tokio::test]
    async fn alias_descendant_folder_search_is_case_and_diacritic_insensitive() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut spam = message("Spam child", "");
        spam.id = "spam-alias-child".into();
        spam.account_id = account_id.to_string();
        spam.thread_id = spam.id.clone();
        spam.mailbox = "Spam::Bülk/Child".into();
        let mut trash = message("Trash child", "");
        trash.id = "trash-alias-child".into();
        trash.account_id = account_id.to_string();
        trash.thread_id = trash.id.clone();
        trash.uid = 2;
        trash.mailbox = "Trash::Bîn/Child".into();
        store
            .upsert_messages(&[spam.clone(), trash.clone()])
            .await
            .unwrap();

        for (text, expected) in [
            ("in:bulk/*", spam.id.clone()),
            ("in:BIN/*", trash.id.clone()),
        ] {
            let query = SearchQuery {
                text: text.into(),
                account_ids: vec![account_id],
                ..Default::default()
            };
            assert_eq!(
                store.search(&query).await.unwrap()[0].id,
                expected,
                "flat {text}"
            );
            assert_eq!(
                store
                    .search_conversation_page(&query)
                    .await
                    .unwrap()
                    .conversations[0]
                    .latest
                    .id,
                expected,
                "conversation {text}"
            );
        }
    }

    #[tokio::test]
    async fn selectable_mailboxes_keep_opaque_identity_and_memberships_account_scoped() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "mailbox-catalogue@dakia.dev".into(),
            display_name: "Mailbox catalogue".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let parent = store
            .upsert_selectable_mailbox(
                account.id,
                &SelectableMailboxDraft {
                    remote_path: "Projects".into(),
                    local_path: Some("Projects".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: false,
                    uid_validity: Some(7),
                    catalogue_coverage: "partial".into(),
                },
            )
            .await
            .unwrap();
        let child_draft = SelectableMailboxDraft {
            remote_path: "Projects/2026".into(),
            local_path: Some("Projects/2026".into()),
            hierarchy_delimiter: Some("/".into()),
            parent_id: Some(parent.id.clone()),
            parent_path: Some("Projects".into()),
            special_use: Some("archive".into()),
            selectable: true,
            uid_validity: Some(9),
            catalogue_coverage: "complete".into(),
        };
        let child = store
            .upsert_selectable_mailbox(account.id, &child_draft)
            .await
            .unwrap();
        let refreshed = store
            .upsert_selectable_mailbox(account.id, &child_draft)
            .await
            .unwrap();
        assert_eq!(child.id, refreshed.id, "upsert must retain the local ID");
        assert_eq!(
            store
                .selectable_mailbox_catalogue(account.id)
                .await
                .unwrap()
                .len(),
            2
        );

        let mut row = message("Membership", "body");
        row.id = "membership-message".into();
        row.account_id = account.id.to_string();
        store.upsert_messages(&[row.clone()]).await.unwrap();
        store
            .set_message_mailbox_memberships(
                account.id,
                &row.id,
                &[parent.id.clone(), child.id.clone(), child.id.clone()],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .list_message_mailbox_memberships(account.id, &row.id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(store
            .delete_selectable_mailbox(account.id, &child.id)
            .await
            .unwrap());
        assert_eq!(
            store
                .list_message_mailbox_memberships(account.id, &row.id)
                .await
                .unwrap()
                .len(),
            1,
            "deleting a mailbox cascades only that membership"
        );
        store.delete_account(account.id).await.unwrap();
        assert!(store
            .selectable_mailbox_catalogue(account.id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .list_message_mailbox_memberships(account.id, &row.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn authoritative_list_retirement_removes_omitted_mailbox_cache_state_and_memberships() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "INBOX".into(),
                    local_path: Some("INBOX".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: Some("\\Inbox".into()),
                    selectable: true,
                    uid_validity: Some(1),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let retired = store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Projects/2026".into(),
                    local_path: Some("Projects/2026".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: Some("Projects".into()),
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(9),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let storage = generic_mailbox_storage_identity("Projects/2026", "Projects/2026");
        let mut row = message("Retired project", "must disappear");
        row.id = "retired-project-message".into();
        row.account_id = account_id.to_string();
        row.mailbox = storage.clone();
        store.upsert_messages(&[row.clone()]).await.unwrap();
        store
            .set_message_mailbox_memberships(account_id, &row.id, std::slice::from_ref(&retired.id))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, &storage, "Projects/2026", 9, 1, true)
            .await
            .unwrap();

        assert_eq!(
            store
                .retire_selectable_mailboxes_absent_from_authoritative_list(
                    account_id,
                    &HashSet::from(["INBOX".to_owned()]),
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .list_selectable_mailboxes(account_id)
                .await
                .unwrap()
                .into_iter()
                .map(|mailbox| mailbox.remote_path)
                .collect::<Vec<_>>(),
            vec!["INBOX"]
        );
        assert!(store.message(&row.id).await.unwrap().is_none());
        assert!(store
            .mailbox_catalog_state(account_id, &storage)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .list_message_mailbox_memberships(account_id, &row.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn folder_search_uses_logical_memberships_across_rfc_message_id_aliases() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let clients = store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Clients".into(),
                    local_path: Some("Clients".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(1),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();
        let invoices = store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Invoices".into(),
                    local_path: Some("Invoices".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(2),
                    catalogue_coverage: "complete".into(),
                },
            )
            .await
            .unwrap();

        let mut inbox_copy = message("Shared invoice", "body");
        inbox_copy.id = "logical-inbox-copy".into();
        inbox_copy.account_id = account_id.to_string();
        inbox_copy.mailbox = "Spam::Bulk".into();
        inbox_copy.message_id = Some(" <shared@example.test> ".into());
        inbox_copy.thread_id = inbox_copy.id.clone();
        let mut archive_copy = inbox_copy.clone();
        archive_copy.id = "logical-archive-copy".into();
        archive_copy.mailbox = "Archive".into();
        archive_copy.uid = 2;
        archive_copy.message_id = Some("<SHARED@example.test>".into());
        archive_copy.thread_id = archive_copy.id.clone();
        store
            .upsert_messages(&[inbox_copy.clone(), archive_copy.clone()])
            .await
            .unwrap();
        store
            .set_message_mailbox_memberships(
                account_id,
                &inbox_copy.id,
                std::slice::from_ref(&clients.id),
            )
            .await
            .unwrap();
        store
            .set_message_mailbox_memberships(
                account_id,
                &archive_copy.id,
                std::slice::from_ref(&invoices.id),
            )
            .await
            .unwrap();
        let provider_paths = store
            .logical_mailbox_paths_by_message_ids(std::slice::from_ref(&inbox_copy.id))
            .await
            .unwrap();
        assert_eq!(
            provider_paths.get(&inbox_copy.id),
            Some(&vec!["Clients".into(), "Invoices".into()]),
            "provider filtering receives memberships persisted by an earlier physical alias"
        );

        async fn matching_ids(store: &Store, account_id: AccountId, text: &str) -> HashSet<String> {
            store
                .search(&SearchQuery {
                    text: text.into(),
                    account_ids: vec![account_id],
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_iter()
                .map(|message| message.id)
                .collect::<HashSet<_>>()
        }
        let expected = HashSet::from([inbox_copy.id.clone(), archive_copy.id.clone()]);
        assert_eq!(
            matching_ids(&store, account_id, "in:Clients in:Invoices").await,
            expected
        );
        let conversations = store
            .search_conversations(&SearchQuery {
                text: "in:Clients in:Invoices".into(),
                account_ids: vec![account_id],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            conversations.iter().any(|conversation| {
                conversation
                    .source_messages
                    .iter()
                    .any(|message| message.id == inbox_copy.id)
            }),
            "a logical label must retain a matching physical Spam alias in conversation results"
        );
        assert_eq!(
            matching_ids(&store, account_id, "in:\"Clients\" AND (in:Invoices)").await,
            expected,
            "quoted and nested folder scopes retain logical AND semantics"
        );

        // Replacing one physical row's provider result must not remove a
        // membership reported for its RFC Message-ID sibling.
        store
            .set_message_mailbox_memberships(account_id, &inbox_copy.id, &[clients.id])
            .await
            .unwrap();
        assert_eq!(
            store
                .list_message_mailbox_memberships(account_id, &archive_copy.id)
                .await
                .unwrap()
                .into_iter()
                .map(|membership| membership.mailbox_id)
                .collect::<Vec<_>>(),
            vec![invoices.id]
        );
        assert_eq!(
            matching_ids(&store, account_id, "in:Clients in:Invoices").await,
            expected
        );
    }

    #[tokio::test]
    async fn deduplicates_message_id_copies_in_a_conversation() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("One message", "Inbox copy");
        inbox.id = "inbox-copy".into();
        inbox.account_id = account_id.to_string();
        inbox.message_id = Some("<same@example.com>".into());
        let mut archive = inbox.clone();
        archive.id = "archive-copy".into();
        archive.mailbox = "Archive".into();
        archive.uid = 2;
        store.upsert_messages(&[inbox, archive]).await.unwrap();
        let conversations = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(conversations[0].message_count, 1);
        assert_eq!(conversations[0].messages[0].mailbox, "INBOX");
        assert_eq!(conversations[0].source_messages.len(), 2);
        assert_eq!(
            conversations[0]
                .source_messages
                .iter()
                .map(|message| (message.id.as_str(), message.mailbox.as_str(), message.uid))
                .collect::<HashSet<_>>(),
            HashSet::from([("inbox-copy", "INBOX", 1), ("archive-copy", "Archive", 2)])
        );
        // The UI shows the selected mailbox representative, while mutation
        // callers receive both concrete account/mailbox/UID locators.
        assert_eq!(conversations[0].messages.len(), 1);
        assert_eq!(conversations[0].messages[0].id, "inbox-copy");
        let serialized = serde_json::to_value(&conversations[0]).unwrap();
        assert_eq!(serialized["sourceMessages"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn conversation_target_falls_back_after_a_move_and_keeps_copy_locators() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("Topic", "Inbox copy");
        inbox.id = "inbox-copy".into();
        inbox.account_id = account_id.to_string();
        inbox.message_id = Some("<root@example.test>".into());
        let mut archive = inbox.clone();
        archive.id = "archive-copy".into();
        archive.mailbox = "Archive".into();
        archive.uid = 2;
        let mut reply = message("Re: Topic", "Reply");
        reply.id = "reply".into();
        reply.account_id = account_id.to_string();
        reply.uid = 3;
        reply.message_id = Some("<reply@example.test>".into());
        reply.in_reply_to = Some("<root@example.test>".into());
        store
            .upsert_messages(&[inbox, archive, reply])
            .await
            .unwrap();
        let newer_threads = (0..120)
            .map(|index| {
                let mut newer = message("Newer", "Unrelated conversation");
                newer.id = format!("newer-{index}");
                newer.account_id = account_id.to_string();
                newer.uid = 100 + index;
                newer.message_id = Some(format!("<newer-{index}@example.test>"));
                newer.thread_id = format!("newer-thread-{index}");
                newer.received_at += chrono::Duration::minutes(index + 1);
                newer
            })
            .collect::<Vec<_>>();
        store.upsert_messages(&newer_threads).await.unwrap();

        let by_local = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: Some("inbox-copy".into()),
                rfc_message_id: None,
                thread_id: None,
                mailbox: Some("INBOX".into()),
            })
            .await
            .unwrap()
            .expect("local target resolves");
        assert_eq!(by_local.messages.len(), 2);
        assert_eq!(by_local.source_messages.len(), 3);
        assert!(by_local
            .source_messages
            .iter()
            .any(|message| message.id == "archive-copy"));

        sqlx::query("DELETE FROM messages WHERE id = 'inbox-copy'")
            .execute(&store.pool)
            .await
            .unwrap();

        let by_rfc = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: Some("stale-local-id-after-move".into()),
                rfc_message_id: Some(" <ROOT@EXAMPLE.TEST> ".into()),
                thread_id: None,
                mailbox: Some("INBOX".into()),
            })
            .await
            .unwrap()
            .expect("RFC Message-ID fallback resolves");
        assert_eq!(by_rfc.id, by_local.id);
        assert_eq!(by_rfc.source_messages.len(), 2);

        let by_thread = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: None,
                rfc_message_id: None,
                thread_id: Some(by_local.thread_id.clone()),
                mailbox: Some("INBOX".into()),
            })
            .await
            .unwrap()
            .expect("thread fallback resolves");
        assert_eq!(by_thread.id, by_local.id);
    }

    #[tokio::test]
    async fn conversation_target_rejects_mismatches_and_ambiguous_rfc_ids() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let other_account_id = uuid::Uuid::new_v4();
        let mut account_message = message("Account target", "Owned by account");
        account_message.id = "account-message".into();
        account_message.account_id = account_id.to_string();
        account_message.message_id = Some("<account@example.test>".into());
        let mut other_message = account_message.clone();
        other_message.id = "other-account-message".into();
        other_message.account_id = other_account_id.to_string();
        other_message.uid = 2;
        store
            .upsert_messages(&[account_message.clone(), other_message])
            .await
            .unwrap();
        assert!(store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: Some("other-account-message".into()),
                rfc_message_id: Some("<account@example.test>".into()),
                thread_id: Some(account_message.thread_id.clone()),
                mailbox: None,
            })
            .await
            .unwrap()
            .is_none());

        let mut recycled = account_message.clone();
        recycled.id = "recycled-local-id".into();
        recycled.uid = 9;
        recycled.message_id = Some("<replacement@example.test>".into());
        recycled.thread_id = "replacement-thread".into();
        store.upsert_messages(&[recycled]).await.unwrap();
        let original = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: Some("recycled-local-id".into()),
                rfc_message_id: Some("<account@example.test>".into()),
                thread_id: Some(account_message.thread_id.clone()),
                mailbox: None,
            })
            .await
            .unwrap()
            .expect("recycled local IDs fall back to the durable target");
        assert!(original
            .source_messages
            .iter()
            .any(|message| message.id == "account-message"));
        assert!(!original
            .source_messages
            .iter()
            .any(|message| message.id == "recycled-local-id"));

        let mut first = message("First", "First duplicate");
        first.id = "first-duplicate".into();
        first.account_id = account_id.to_string();
        first.uid = 3;
        first.message_id = Some("<duplicated@example.test>".into());
        let mut second = first.clone();
        second.id = "second-duplicate".into();
        second.uid = 4;
        store.upsert_messages(&[first, second]).await.unwrap();
        // A stale or repaired index can retain duplicate RFC IDs in separate
        // threads. The resolver must not choose either one by an arbitrary
        // query order, even when the caller also supplies a thread fallback.
        sqlx::query("UPDATE messages SET thread_id = 'other-thread' WHERE id = 'second-duplicate'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: None,
                rfc_message_id: Some("<DUPLICATED@example.test>".into()),
                thread_id: Some("other-thread".into()),
                mailbox: None,
            })
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn conversation_target_keeps_spam_family_isolated() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut inbox = message("Topic", "Inbox");
        inbox.id = "inbox".into();
        inbox.account_id = account_id.to_string();
        inbox.message_id = Some("<inbox@example.test>".into());
        let mut spam = message("Re: Topic", "Spam reply");
        spam.id = "spam".into();
        spam.account_id = account_id.to_string();
        spam.mailbox = "Spam::Bulk".into();
        spam.uid = 2;
        spam.message_id = Some("<spam@example.test>".into());
        spam.in_reply_to = Some("<inbox@example.test>".into());
        store.upsert_messages(&[inbox, spam]).await.unwrap();

        assert!(store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: None,
                rfc_message_id: Some("<spam@example.test>".into()),
                thread_id: None,
                mailbox: None,
            })
            .await
            .unwrap()
            .is_none());
        let spam_only = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: None,
                rfc_message_id: Some("<spam@example.test>".into()),
                thread_id: None,
                mailbox: Some("Spam".into()),
            })
            .await
            .unwrap()
            .expect("explicit Spam target resolves");
        assert_eq!(spam_only.messages.len(), 1);
        assert_eq!(spam_only.source_messages.len(), 1);
        let nested_spam = store
            .conversation_for_target(&ConversationTarget {
                account_id,
                local_message_id: Some("spam".into()),
                rfc_message_id: None,
                thread_id: None,
                mailbox: Some("Spam::Bulk".into()),
            })
            .await
            .unwrap()
            .expect("nested Spam target resolves within its family");
        assert_eq!(nested_spam.messages.len(), 1);
        assert_eq!(spam_only.messages[0].mailbox, "Spam::Bulk");
    }

    #[tokio::test]
    async fn threading_header_backfill_is_resumable_and_rebuilds_groups() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut root = message("Topic", "Root");
        root.id = "root".into();
        root.account_id = account_id.to_string();
        root.uid = 1;
        root.message_id = Some("<root@example.com>".into());
        let mut reply = message("Re: Topic", "Reply");
        reply.id = "reply".into();
        reply.account_id = account_id.to_string();
        reply.uid = 2;
        store.upsert_messages(&[root, reply]).await.unwrap();
        sqlx::query("UPDATE messages SET threading_scanned = 0 WHERE id = 'reply'")
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(
            store
                .unscanned_threading_uids(account_id, "INBOX", 1)
                .await
                .unwrap(),
            [2]
        );
        store
            .save_threading_headers(
                account_id,
                "INBOX",
                2,
                &ThreadingHeaders {
                    message_id: Some("<reply@example.com>".into()),
                    in_reply_to: Some("<root@example.com>".into()),
                    reference_ids: Some("<root@example.com>".into()),
                },
            )
            .await
            .unwrap();
        store.finish_threading_backfill(account_id).await.unwrap();
        assert!(store
            .unscanned_threading_uids(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_empty());
        let rows = store.search(&SearchQuery::default()).await.unwrap();
        assert_eq!(rows[0].thread_id, rows[1].thread_id);
        // Re-running a completed backfill is a no-op.
        store.finish_threading_backfill(account_id).await.unwrap();
        assert_eq!(
            store.search(&SearchQuery::default()).await.unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn unchanged_thread_rebuild_skips_database_updates() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut item = message("Already grouped", "Body");
        item.id = "stable-thread".into();
        item.thread_id = "<stable-thread@example.com>".into();
        item.message_id = Some("<stable-thread@example.com>".into());
        item.account_id = account_id.to_string();
        store.upsert_messages(&[item]).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_redundant_thread_update BEFORE UPDATE OF thread_id ON messages BEGIN SELECT RAISE(ABORT, 'thread id update was unnecessary'); END",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.finish_threading_backfill(account_id).await.unwrap();
    }

    #[tokio::test]
    async fn groups_reference_chains_even_when_messages_arrive_out_of_order() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let mut root = message("Project update", "Root");
        root.id = "root".into();
        root.account_id = account_id.clone();
        root.uid = 1;
        root.message_id = Some("<root@example.com>".into());
        root.thread_id = root.id.clone();
        let mut reply = message("Re: Project update", "Reply");
        reply.id = "reply".into();
        reply.account_id = account_id.clone();
        reply.uid = 2;
        reply.message_id = Some("<reply@example.com>".into());
        reply.in_reply_to = Some("<root@example.com>".into());
        reply.thread_id = reply.id.clone();
        let mut deep_reply = message("Re: Project update", "Another reply");
        deep_reply.id = "deep".into();
        deep_reply.account_id = account_id;
        deep_reply.uid = 3;
        deep_reply.message_id = Some("<deep@example.com>".into());
        deep_reply.in_reply_to = Some("<reply@example.com>".into());
        deep_reply.reference_ids = Some("<root@example.com> <reply@example.com>".into());
        deep_reply.thread_id = deep_reply.id.clone();

        store
            .upsert_messages(&[deep_reply, root, reply])
            .await
            .unwrap();
        let results = store.search(&SearchQuery::default()).await.unwrap();
        let thread_ids = results
            .iter()
            .map(|message| message.thread_id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(thread_ids.len(), 1);
        assert_eq!(thread_ids.into_iter().next(), Some("root@example.com"));
    }

    #[tokio::test]
    async fn groups_sibling_replies_when_the_referenced_root_is_not_local() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let mut first = message("Re: Older conversation", "First reply");
        first.account_id = account_id.clone();
        first.uid = 1;
        first.message_id = Some("<first@example.com>".into());
        first.reference_ids = Some("<missing-root@example.com>".into());
        let mut second = message("Re: Older conversation", "Second reply");
        second.account_id = account_id;
        second.uid = 2;
        second.message_id = Some("<second@example.com>".into());
        second.reference_ids = Some("<missing-root@example.com>".into());

        store.upsert_messages(&[first, second]).await.unwrap();
        let results = store.search(&SearchQuery::default()).await.unwrap();
        assert_eq!(results[0].thread_id, "missing-root@example.com");
        assert_eq!(results[1].thread_id, "missing-root@example.com");
    }

    #[tokio::test]
    async fn subject_fallback_requires_a_reply_and_shared_participants() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let mut original = message("Launch plan", "Original");
        original.account_id = account_id.clone();
        original.uid = 1;
        let mut reply = message("Re: Launch plan", "Reply without headers");
        reply.account_id = account_id.clone();
        reply.uid = 2;
        let mut unrelated = message("Re: Launch plan", "Unrelated");
        unrelated.account_id = account_id;
        unrelated.uid = 3;
        unrelated.from_address = "stranger@example.net".into();
        unrelated.to_addresses = "someone-else@example.net".into();

        store
            .upsert_messages(&[original, reply, unrelated])
            .await
            .unwrap();
        let results = store.search(&SearchQuery::default()).await.unwrap();
        let original_thread = &results.iter().find(|item| item.uid == 1).unwrap().thread_id;
        assert_eq!(
            results.iter().find(|item| item.uid == 2).unwrap().thread_id,
            *original_thread
        );
        assert_ne!(
            results.iter().find(|item| item.uid == 3).unwrap().thread_id,
            *original_thread
        );
    }

    #[tokio::test]
    async fn identical_newsletter_subjects_do_not_merge_without_headers() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let mut first = message("Daily briefing", "Monday");
        first.account_id = account_id.clone();
        first.uid = 1;
        let mut second = message("Daily briefing", "Tuesday");
        second.account_id = account_id;
        second.uid = 2;
        second.received_at += chrono::Duration::days(1);
        store.upsert_messages(&[first, second]).await.unwrap();
        let rows = store.search(&SearchQuery::default()).await.unwrap();
        assert_ne!(rows[0].thread_id, rows[1].thread_id);
    }

    #[tokio::test]
    async fn thread_identity_stays_stable_when_older_ancestors_arrive() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4().to_string();
        let mut child = message("Re: Long thread", "Child");
        child.id = "child".into();
        child.account_id = account_id.clone();
        child.message_id = Some("<child@example.com>".into());
        child.in_reply_to = Some("<parent@example.com>".into());
        child.reference_ids = Some("<root@example.com> <parent@example.com>".into());
        store.upsert_messages(&[child]).await.unwrap();
        let initial = store.search(&SearchQuery::default()).await.unwrap()[0]
            .thread_id
            .clone();

        let mut root = message("Long thread", "Root");
        root.id = "root".into();
        root.account_id = account_id.clone();
        root.uid = 2;
        root.message_id = Some("<root@example.com>".into());
        root.received_at -= chrono::Duration::days(2);
        let mut parent = message("Re: Long thread", "Parent");
        parent.id = "parent".into();
        parent.account_id = account_id;
        parent.uid = 3;
        parent.message_id = Some("<parent@example.com>".into());
        parent.in_reply_to = Some("<root@example.com>".into());
        parent.reference_ids = Some("<root@example.com>".into());
        parent.received_at -= chrono::Duration::days(1);
        store.upsert_messages(&[root, parent]).await.unwrap();
        let rows = store.search(&SearchQuery::default()).await.unwrap();
        assert!(rows.iter().all(|message| message.thread_id == initial));
        assert_eq!(initial, "root@example.com");
    }

    #[tokio::test]
    async fn thread_graph_never_crosses_account_boundaries() {
        let store = Store::in_memory().await.unwrap();
        let mut first = message("Shared id", "First account");
        first.id = "first-account".into();
        first.account_id = uuid::Uuid::new_v4().to_string();
        first.message_id = Some("<same@example.com>".into());
        let mut second = message("Shared id", "Second account");
        second.id = "second-account".into();
        second.account_id = uuid::Uuid::new_v4().to_string();
        second.message_id = Some("<same@example.com>".into());
        store.upsert_messages(&[first, second]).await.unwrap();
        let conversations = store
            .search_conversations(&SearchQuery::default())
            .await
            .unwrap();
        assert_eq!(conversations.len(), 2);
        assert!(conversations
            .iter()
            .all(|conversation| conversation.message_count == 1));
    }

    #[tokio::test]
    async fn fixed_seed_thread_graphs_are_invariant_to_insert_and_chunk_order_across_accounts() {
        let first_account = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0101);
        let second_account = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0102);
        let base_time = DateTime::<Utc>::UNIX_EPOCH;
        let mut graph = Vec::new();
        for (account_id, prefix) in [(first_account, "first"), (second_account, "second")] {
            let mut root = message("Quarterly plan", "root");
            root.id = format!("{prefix}-root");
            root.account_id = account_id.to_string();
            root.uid = 1;
            root.message_id = Some("<shared-root@example.test>".into());
            root.received_at = base_time;

            let mut reply = message("Re: Quarterly plan", "reply");
            reply.id = format!("{prefix}-reply");
            reply.account_id = account_id.to_string();
            reply.uid = 2;
            reply.message_id = Some("<shared-reply@example.test>".into());
            reply.in_reply_to = Some("<shared-root@example.test>".into());
            reply.reference_ids = Some("<shared-root@example.test>".into());
            reply.received_at = base_time + chrono::Duration::seconds(1);

            let mut deep_reply = message("Re: Quarterly plan", "deep reply");
            deep_reply.id = format!("{prefix}-deep-reply");
            deep_reply.account_id = account_id.to_string();
            deep_reply.uid = 3;
            deep_reply.message_id = Some("<shared-deep-reply@example.test>".into());
            deep_reply.in_reply_to = Some("<shared-reply@example.test>".into());
            deep_reply.reference_ids =
                Some("<shared-root@example.test> <shared-reply@example.test>".into());
            deep_reply.received_at = base_time + chrono::Duration::seconds(2);
            graph.extend([root, reply, deep_reply]);
        }

        let first = Store::in_memory().await.unwrap();
        for chunk in fixed_seed_order(graph.len(), 0x7e57_1e55).chunks(2) {
            let messages = chunk
                .iter()
                .map(|index| graph[*index].clone())
                .collect::<Vec<_>>();
            first.upsert_messages(&messages).await.unwrap();
        }
        let second = Store::in_memory().await.unwrap();
        for chunk in fixed_seed_order(graph.len(), 0x9b5d_cafe).chunks(3) {
            let messages = chunk
                .iter()
                .map(|index| graph[*index].clone())
                .collect::<Vec<_>>();
            second.upsert_messages(&messages).await.unwrap();
        }

        let mut first_threads = first
            .search(&SearchQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|message| (message.id, message.thread_id))
            .collect::<Vec<_>>();
        let mut second_threads = second
            .search(&SearchQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|message| (message.id, message.thread_id))
            .collect::<Vec<_>>();
        first_threads.sort();
        second_threads.sort();
        assert_eq!(first_threads, second_threads);
        assert!(first_threads
            .iter()
            .all(|(_, thread_id)| thread_id == "shared-root@example.test"));
        assert_eq!(
            first
                .search_conversations(&SearchQuery::default())
                .await
                .unwrap()
                .len(),
            2,
            "same RFC message ids must remain isolated by account"
        );
    }

    #[tokio::test]
    async fn migration_groups_existing_reply_shaped_messages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite3");
        let options = SqliteConnectOptions::from_str(path.to_str().unwrap())
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let account = AccountDraft {
            email: "legacy-threading@dakia.dev".into(),
            display_name: "Legacy threading".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, created_at TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?)")
            .bind(account.id.to_string())
            .bind(&account.email)
            .bind(serde_json::to_string(&account).unwrap())
            .bind(account.created_at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, message_id TEXT, subject TEXT NOT NULL, from_name TEXT, from_address TEXT NOT NULL, to_addresses TEXT NOT NULL, received_at TEXT NOT NULL, snippet TEXT NOT NULL, body_text TEXT NOT NULL, body_html TEXT, unsubscribe_kind TEXT, unsubscribe_url TEXT, unsubscribe_scanned INTEGER NOT NULL DEFAULT 0, is_read INTEGER NOT NULL DEFAULT 0, is_flagged INTEGER NOT NULL DEFAULT 0, has_attachments INTEGER NOT NULL DEFAULT 0, UNIQUE(account_id, mailbox, uid))")
            .execute(&pool)
            .await
            .unwrap();
        for (id, uid, subject) in [
            ("legacy-root", 1, "Migration"),
            ("legacy-reply", 2, "Re: Migration"),
        ] {
            sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, subject, from_address, to_addresses, received_at, snippet, body_text) VALUES (?, ?, 'INBOX', ?, ?, ?, 'mara@example.com', 'you@example.com', ?, 'legacy preview', 'legacy full body')")
                .bind(id)
                .bind(account.id.to_string())
                .bind(uid)
                .bind(format!("<{id}@example.com>"))
                .bind(subject)
                .bind(Utc::now())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("CREATE VIRTUAL TABLE messages_fts USING fts5(subject, from_name, from_address, to_addresses, body_text, content='messages', content_rowid='rowid')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages_fts(messages_fts) VALUES ('rebuild')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let store = Store::open(&path).await.unwrap();
        let results = store.search(&SearchQuery::default()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].thread_id, results[1].thread_id);
        assert!(results.iter().all(|message| message.body_text.is_empty()));
        assert!(results.iter().all(|message| {
            message.cc_addresses.is_empty()
                && message.bcc_addresses.is_empty()
                && message.reply_to_addresses.is_empty()
        }));
        let recipient_columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('messages') WHERE name IN ('cc_addresses', 'bcc_addresses', 'recipient_headers_scanned', 'reply_to_addresses') ORDER BY name")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            recipient_columns,
            [
                "bcc_addresses",
                "cc_addresses",
                "recipient_headers_scanned",
                "reply_to_addresses"
            ]
        );
        assert_eq!(
            store
                .unscanned_recipient_header_uids(account.id, "INBOX", 10)
                .await
                .unwrap(),
            [2, 1]
        );
        // This is the durable end of a normal-sync header backfill: an
        // upgraded row receives the actual values, while an observed absence
        // is also terminal and will not be fetched again.
        store
            .save_recipient_headers(
                account.id,
                "INBOX",
                2,
                "Copy <copy@example.com>",
                "Hidden <hidden@example.com>",
                "Replies <replies@example.com>",
            )
            .await
            .unwrap();
        store
            .save_recipient_headers(account.id, "INBOX", 1, "", "", "")
            .await
            .unwrap();
        let upgraded = store
            .message_by_locator(account.id, "INBOX", 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(upgraded.cc_addresses, "Copy <copy@example.com>");
        assert_eq!(upgraded.bcc_addresses, "Hidden <hidden@example.com>");
        assert_eq!(upgraded.reply_to_addresses, "Replies <replies@example.com>");
        assert!(store
            .unscanned_recipient_header_uids(account.id, "INBOX", 10)
            .await
            .unwrap()
            .is_empty());
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE threading_scanned = 0")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(pending, 2);
    }

    #[tokio::test]
    async fn migrates_the_original_desktop_profile_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dakia.db");
        let options = SqliteConnectOptions::from_str(path.to_str().unwrap())
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, displayName TEXT, host TEXT NOT NULL, port INTEGER NOT NULL, tls INTEGER NOT NULL, username TEXT NOT NULL, providerCapabilities TEXT NOT NULL, createdAt TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE mailboxes (id TEXT PRIMARY KEY, accountId TEXT NOT NULL, path TEXT NOT NULL, uidValidity INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, accountId TEXT NOT NULL, mailboxId TEXT NOT NULL, threadId TEXT, uid INTEGER NOT NULL, messageId TEXT, inReplyTo TEXT, referencesJson TEXT NOT NULL, fromAddress TEXT NOT NULL, toAddresses TEXT NOT NULL, subject TEXT, date TEXT, flags TEXT NOT NULL, snippet TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        let account_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO accounts(id, email, displayName, host, port, tls, username, providerCapabilities, createdAt) VALUES (?, 'person@example.test', 'Person', 'imap.migadu.com', 993, 1, 'person@example.test', '{\"archiveMailbox\":\"Archive\",\"spamMailbox\":\"Junk\"}', '2026-01-01 00:00:00')")
            .bind(account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailboxes(id, accountId, path, uidValidity) VALUES ('inbox', ?, 'INBOX', 42)")
            .bind(account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailboxes(id, accountId, path, uidValidity) VALUES ('sent', ?, 'Sent', 43)")
            .bind(account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(id, accountId, mailboxId, threadId, uid, messageId, inReplyTo, referencesJson, fromAddress, toAddresses, subject, date, flags, snippet) VALUES ('legacy-message', ?, 'inbox', NULL, 5, '<legacy@example.test>', NULL, '[]', 'sender@example.test', 'person@example.test', 'Legacy subject', 'Mon, 01 Jan 2024 12:00:00 +0000', '[\"\\\\Seen\"]', 'Legacy preview')")
            .bind(account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(id, accountId, mailboxId, threadId, uid, messageId, inReplyTo, referencesJson, fromAddress, toAddresses, subject, date, flags, snippet) VALUES ('legacy-sent', ?, 'sent', NULL, 6, '<legacy-sent@example.test>', NULL, '[]', 'person@example.test', 'Sender <sender@example.test>', 'Prior reply', 'Mon, 01 Jan 2024 13:00:00 +0000', '[\"\\\\Seen\"]', 'My reply')")
            .bind(account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let store = Store::open(&path).await.unwrap();
        let accounts = store.accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, account_id);
        assert_eq!(accounts[0].provider_id, "migadu");
        assert_eq!(accounts[0].smtp_host, "smtp.migadu.com");
        let messages = store
            .search(&SearchQuery {
                text: "Legacy".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "legacy-message");
        assert_eq!(messages[0].content_state, "headers_only");
        assert!(messages[0].is_read);
        assert_eq!(
            store
                .messages_from_known_correspondents(&messages)
                .await
                .unwrap(),
            HashSet::from(["legacy-message".into()])
        );
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.uid_validity, 42);
        store.pool.close().await;
    }

    #[tokio::test]
    async fn catalogue_never_persists_attachment_or_body_bytes() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Invoice", "See attachment");
        message.has_attachments = true;
        message.attachments.push(AttachmentData {
            attachment: Attachment {
                id: format!("{}:0", message.id),
                message_id: message.id.clone(),
                filename: "invoice.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 3,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            },
            bytes: b"pdf".to_vec(),
        });
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        let catalogue: Vec<(String, String, i64, String)> = sqlx::query_as(
            "SELECT filename, mime_type, size_bytes, presentation FROM message_attachment_catalogue WHERE message_id = ?",
        )
        .bind(&message.id)
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            catalogue,
            vec![(
                "invoice.pdf".into(),
                "application/pdf".into(),
                3,
                "downloadable".into(),
            )]
        );
        let matches = store
            .search(&SearchQuery {
                text: "has:attachment filename:invoice* filetype:pdf".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            matches.iter().map(|row| &row.id).collect::<Vec<_>>(),
            vec![&message.id]
        );
        let listed = store.attachments(&message.id).await.unwrap();
        assert!(listed.is_empty());
        let stored = store
            .messages_by_ids(&[message.id.clone()])
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(stored.body_text.is_empty());
        assert!(stored.body_html.is_none());

        message.has_attachments = false;
        message.attachments.clear();
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_attachment_catalogue WHERE message_id = ?",
        )
        .bind(&message.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            remaining, 0,
            "an authoritative refresh replaces stale metadata"
        );
    }

    #[tokio::test]
    async fn attachment_catalogue_keeps_distinct_parts_with_identical_visible_metadata() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Duplicate-looking MIME parts", "preview");
        message.has_attachments = true;
        for (id, presentation) in [
            ("part-embedded", AttachmentPresentation::Embedded),
            ("part-download", AttachmentPresentation::Downloadable),
        ] {
            message.attachments.push(AttachmentData {
                attachment: Attachment {
                    id: id.into(),
                    message_id: message.id.clone(),
                    filename: "logo.png".into(),
                    mime_type: "image/png".into(),
                    size_bytes: 42,
                    is_inline: false,
                    presentation,
                    is_potentially_unsafe: false,
                },
                bytes: Vec::new(),
            });
        }
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        let catalogue: Vec<(String, String)> = sqlx::query_as(
            "SELECT attachment_id, presentation FROM message_attachment_catalogue WHERE message_id = ? ORDER BY attachment_id",
        )
        .bind(&message.id)
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            catalogue,
            vec![
                ("part-download".into(), "downloadable".into()),
                ("part-embedded".into(), "embedded".into()),
            ]
        );
    }

    #[tokio::test]
    async fn search_body_text_cache_is_searchable_without_completing_the_reader() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Remote result", "");
        message.content_state = "headers_only".into();
        message.body_html = None;
        message.has_attachments = true;
        message.attachments.push(AttachmentData {
            attachment: Attachment {
                id: format!("{}:0", message.id),
                message_id: message.id.clone(),
                filename: "contract.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 7,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            },
            bytes: b"ignored".to_vec(),
        });
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();

        assert!(store
            .cache_search_body_text(&message.id, "Confidential search-only phrase")
            .await
            .unwrap());
        assert_eq!(
            store.cached_search_body_text(&message.id).await.unwrap(),
            Some("Confidential search-only phrase".into())
        );
        assert_eq!(
            store
                .search(&SearchQuery {
                    text: "body:\"search-only phrase\"".into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .iter()
                .map(|row| &row.id)
                .collect::<Vec<_>>(),
            vec![&message.id]
        );

        let reader_message = store.message(&message.id).await.unwrap().unwrap();
        assert_eq!(reader_message.content_state, "headers_only");
        assert!(reader_message.body_text.is_empty());
        assert!(reader_message.body_html.is_none());
        assert!(store
            .cached_message_content(&message.id)
            .await
            .unwrap()
            .is_none());
        let reader_cache_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache WHERE message_id = ?")
                .bind(&message.id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(reader_cache_rows, 0);
        let search_cache_columns: (String, i64) = sqlx::query_as(
            "SELECT body_text, byte_size FROM message_search_body_text WHERE message_id = ?",
        )
        .bind(&message.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(search_cache_columns.0, "Confidential search-only phrase");
        assert_eq!(
            search_cache_columns.1,
            i64::try_from("Confidential search-only phrase".len()).unwrap()
        );
    }

    #[tokio::test]
    async fn local_body_coverage_stays_partial_for_headers_only_messages_after_v2_completion() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut headers_only = message("Headers only", "");
        headers_only.account_id = account_id.to_string();
        headers_only.id = "coverage-headers".into();
        headers_only.content_state = "headers_only".into();
        let mut search_body = headers_only.clone();
        search_body.id = "coverage-search".into();
        search_body.uid = 2;
        let mut reader_body = headers_only.clone();
        reader_body.id = "coverage-reader".into();
        reader_body.uid = 3;
        let mut starred_body = headers_only.clone();
        starred_body.id = "coverage-starred".into();
        starred_body.uid = 4;
        starred_body.is_flagged = true;
        store
            .upsert_messages(&[
                headers_only.clone(),
                search_body.clone(),
                reader_body.clone(),
                starred_body.clone(),
            ])
            .await
            .unwrap();
        store
            .cache_search_body_text(&search_body.id, "search body")
            .await
            .unwrap();
        store
            .cache_message_content(&reader_body.id, false, cached_content("reader body"))
            .await
            .unwrap();
        store
            .cache_starred_message_content(&starred_body.id, cached_content("starred body"))
            .await
            .unwrap();

        assert!(store.search_catalogue_v2_complete().await.unwrap());
        assert_eq!(
            store.local_body_index_coverage(account_id).await.unwrap(),
            LocalBodyIndexCoverage {
                account_id: account_id.to_string(),
                catalogue_messages: 4,
                searchable_bodies: 3,
            }
        );
    }

    #[tokio::test]
    async fn attachment_catalogue_presentation_column_migrates_old_rows_as_unknown() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Old catalogue", "Metadata before presentation");
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();

        sqlx::query("DROP TABLE message_attachment_catalogue")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE message_attachment_catalogue (message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(message_id, filename, mime_type, size_bytes, is_inline))")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO message_attachment_catalogue(message_id, filename, mime_type, size_bytes, is_inline) VALUES (?, 'legacy.pdf', 'application/pdf', 3, 0)")
            .bind(&message.id)
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .migrate_attachment_presentation_metadata()
            .await
            .unwrap();
        let presentation: String = sqlx::query_scalar(
            "SELECT presentation FROM message_attachment_catalogue WHERE message_id = ?",
        )
        .bind(&message.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(presentation, "unknown");
    }

    #[tokio::test]
    async fn inline_attachment_catalogue_rows_are_not_searchable_attachments() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Signed message", "Inline signature image only");
        // Keep this true to cover older provider data while the provider
        // worker corrects its paperclip flag. Search must trust the catalogue
        // distinction rather than treating every named inline CID part as a
        // user-facing attachment.
        message.has_attachments = true;
        message.attachments.push(AttachmentData {
            attachment: Attachment {
                id: format!("{}:0", message.id),
                message_id: message.id.clone(),
                filename: "signature.png".into(),
                mime_type: "image/png".into(),
                size_bytes: 42,
                is_inline: true,
                presentation: AttachmentPresentation::Embedded,
                is_potentially_unsafe: false,
            },
            bytes: Vec::new(),
        });
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();

        for query in ["has:attachment", "filename:signature*", "filetype:image"] {
            assert!(
                store
                    .search(&SearchQuery {
                        text: query.into(),
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .is_empty(),
                "{query} must ignore inline-only catalogue metadata"
            );
        }

        let no_attachment = store
            .search(&SearchQuery {
                text: "has:noattachment".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            no_attachment
                .iter()
                .map(|message| &message.id)
                .collect::<Vec<_>>(),
            vec![&message.id],
            "an inline-only catalogue row is not a user-facing attachment"
        );
    }

    #[tokio::test]
    async fn cid_attachments_marked_both_remain_searchable_files() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Shared logo", "The CID image is also downloadable");
        message.has_attachments = true;
        message.attachments.push(AttachmentData {
            attachment: Attachment {
                id: format!("{}:0", message.id),
                message_id: message.id.clone(),
                filename: "brand-logo.png".into(),
                mime_type: "image/png".into(),
                size_bytes: 42,
                is_inline: true,
                presentation: AttachmentPresentation::Both,
                is_potentially_unsafe: false,
            },
            bytes: Vec::new(),
        });
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();

        let presentation: String = sqlx::query_scalar(
            "SELECT presentation FROM message_attachment_catalogue WHERE message_id = ?",
        )
        .bind(&message.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(presentation, "both");

        for query in ["has:attachment", "filename:brand-logo*", "filetype:image"] {
            assert_eq!(
                store
                    .search(&SearchQuery {
                        text: query.into(),
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .iter()
                    .map(|row| &row.id)
                    .collect::<Vec<_>>(),
                vec![&message.id],
                "{query} must retain a CID file explicitly offered for download"
            );
        }
        assert!(store
            .search(&SearchQuery {
                text: "has:noattachment".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn legacy_unknown_attachment_metadata_only_establishes_attachment_presence() {
        let store = Store::in_memory().await.unwrap();
        let mut message = message("Legacy attachment", "Old metadata");
        message.has_attachments = true;
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        // Simulate a pre-presentation catalogue row. Its name/type came from
        // a transport disposition, so only the existing non-inline state is
        // safe to use until an authoritative MIME fetch classifies it.
        sqlx::query("INSERT INTO message_attachment_catalogue(message_id, filename, mime_type, size_bytes, is_inline) VALUES (?, 'possibly-a-logo.png', 'image/png', 42, 0)")
            .bind(&message.id)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(
            store
                .search(&SearchQuery {
                    text: "has:attachment".into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .iter()
                .map(|row| &row.id)
                .collect::<Vec<_>>(),
            vec![&message.id]
        );
        for query in ["filename:possibly-a-logo*", "filetype:image"] {
            assert!(
                store
                    .search(&SearchQuery {
                        text: query.into(),
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .is_empty(),
                "{query} must not trust a legacy unknown filename or MIME type"
            );
        }
    }

    #[tokio::test]
    async fn synced_messages_establish_a_silent_baseline_then_report_new_unread_mail() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut baseline = message("Earlier mail", "Already in the inbox");
        baseline.account_id = account_id.to_string();

        assert!(store
            .save_synced_messages(account_id, "INBOX", &[baseline])
            .await
            .unwrap()
            .is_empty());

        let mut unread = message("New mail", "Notify me");
        unread.account_id = account_id.to_string();
        unread.uid = 2;
        let mut read = message("Already read", "Do not notify me");
        read.account_id = account_id.to_string();
        read.uid = 3;
        read.is_read = true;
        let discovered = store
            .save_synced_messages(account_id, "INBOX", &[unread.clone(), read])
            .await
            .unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].subject, "New mail");
        assert!(store
            .save_synced_messages(account_id, "INBOX", &[unread])
            .await
            .unwrap()
            .is_empty());
        let stored_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE account_id = ? AND mailbox = 'INBOX'",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(stored_count, 3);
    }

    #[tokio::test]
    async fn concurrent_realtime_writes_are_serialized_without_sqlite_busy() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("realtime.db"))
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(21));
        let mut writes = tokio::task::JoinSet::new();
        for index in 0..20 {
            let account_id = uuid::Uuid::new_v4();
            save_test_account(&store, account_id).await;
            let waiting_store = store.clone();
            let waiting_barrier = barrier.clone();
            let mut incoming = message(&format!("Concurrent arrival {index}"), "Notify me");
            incoming.account_id = account_id.to_string();
            writes.spawn(async move {
                waiting_barrier.wait().await;
                waiting_store
                    .save_synced_messages(account_id, "INBOX", &[incoming])
                    .await
            });
        }
        barrier.wait().await;
        while let Some(write) = writes.join_next().await {
            assert!(write.unwrap().unwrap().is_empty());
        }
        let stored: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE subject LIKE 'Concurrent arrival %'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(stored, 20);
    }

    #[tokio::test]
    async fn independent_stores_preserve_live_claims_and_finish_contended_sync_durably() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("independent-stores.db");
        let account_id = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0001);
        let first = Store::open(&path).await.unwrap();
        let account = account_with_id(account_id, "independent-store@example.test");
        first.save_account(&account).await.unwrap();

        let mut first_message = message("Contended 01", "durable payload");
        first_message.id = stable_message_id(account_id, "INBOX", 1);
        first_message.account_id = account_id.to_string();
        first_message.uid = 1;
        first_message.received_at = DateTime::<Utc>::UNIX_EPOCH;
        first
            .save_synced_messages(account_id, "INBOX", &[first_message.clone()])
            .await
            .unwrap();
        let claim = first
            .acquire_message_content_fetch(&first_message.id)
            .await
            .unwrap()
            .expect("first connection owns the fetch");

        let second = Store::open(&path).await.unwrap();
        assert!(matches!(
            second
                .acquire_message_content_fetch_outcome(&first_message.id)
                .await
                .unwrap(),
            MessageContentFetchAcquire::Busy
        ));
        claim.release().await.unwrap();

        let mut batch = vec![first_message];
        for uid in 2..=12 {
            let mut incoming = message(&format!("Contended {uid:02}"), "durable payload");
            incoming.id = stable_message_id(account_id, "INBOX", uid);
            incoming.account_id = account_id.to_string();
            incoming.uid = i64::from(uid);
            incoming.received_at =
                DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(i64::from(uid));
            batch.push(incoming);
        }
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let first_barrier = barrier.clone();
        let first_store = first.clone();
        let first_batch = batch.clone();
        let first_write = tokio::spawn(async move {
            first_barrier.wait().await;
            first_store
                .save_synced_messages(account_id, "INBOX", &first_batch)
                .await
        });
        let second_barrier = barrier.clone();
        let second_store = second.clone();
        let mut second_batch = batch;
        second_batch.reverse();
        let second_write = tokio::spawn(async move {
            second_barrier.wait().await;
            second_store
                .save_synced_messages(account_id, "INBOX", &second_batch)
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            barrier.wait().await;
            first_write.await.unwrap().unwrap();
            second_write.await.unwrap().unwrap();
        })
        .await
        .expect("two independently opened stores must complete their contended writes");

        assert_eq!(
            second
                .search(&SearchQuery {
                    account_ids: vec![account_id],
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            12
        );
        assert_eq!(
            second
                .highest_mailbox_uid(account_id, "INBOX")
                .await
                .unwrap(),
            Some(12)
        );

        drop(second);
        drop(first);
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            reopened
                .message_by_locator(account_id, "INBOX", 1)
                .await
                .unwrap()
                .expect("committed message survives reopening")
                .subject,
            "Contended 01"
        );
        assert_eq!(
            reopened
                .highest_mailbox_uid(account_id, "INBOX")
                .await
                .unwrap(),
            Some(12)
        );
    }

    #[tokio::test]
    async fn concurrent_fresh_store_opens_retry_migration_locking_within_the_busy_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("simultaneous-open.db");
        let barrier = Arc::new(tokio::sync::Barrier::new(5));
        let mut opens = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let path = path.clone();
            let barrier = barrier.clone();
            opens.spawn(async move {
                barrier.wait().await;
                Store::open(path).await
            });
        }

        tokio::time::timeout(Duration::from_secs(10), async {
            barrier.wait().await;
            while let Some(opened) = opens.join_next().await {
                opened.unwrap().unwrap();
            }
        })
        .await
        .expect("fresh Store::open calls must resolve migration contention within 10 seconds");
    }

    #[tokio::test]
    async fn synced_message_batch_rejects_mixed_scope_without_durable_changes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0002);
        let other_account_id = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0003);
        let mut valid = message("Valid input", "must not be partially stored");
        valid.id = stable_message_id(account_id, "INBOX", 1);
        valid.account_id = account_id.to_string();
        valid.uid = 1;
        let mut wrong_account = valid.clone();
        wrong_account.id = stable_message_id(other_account_id, "INBOX", 2);
        wrong_account.account_id = other_account_id.to_string();
        wrong_account.uid = 2;

        let error = store
            .save_synced_messages(account_id, "INBOX", &[valid.clone(), wrong_account])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match the requested account or mailbox"));
        assert!(store
            .message_by_locator(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .highest_mailbox_uid(account_id, "INBOX")
                .await
                .unwrap(),
            None
        );

        let mut wrong_mailbox = valid;
        wrong_mailbox.id = stable_message_id(account_id, "Archive", 3);
        wrong_mailbox.mailbox = "Archive".into();
        wrong_mailbox.uid = 3;
        assert!(store
            .save_synced_messages(account_id, "INBOX", &[wrong_mailbox])
            .await
            .is_err());
        assert!(store
            .message_by_locator(account_id, "Archive", 3)
            .await
            .unwrap()
            .is_none());

        let missing_account_id = uuid::Uuid::from_u128(0x4f40_2cf7_237a_4c70_8d52_6af4_6fd1_0004);
        let mut missing_account = message("Missing account", "must not be stored");
        missing_account.id = stable_message_id(missing_account_id, "INBOX", 4);
        missing_account.account_id = missing_account_id.to_string();
        missing_account.uid = 4;
        assert!(store
            .save_synced_messages(missing_account_id, "INBOX", &[missing_account])
            .await
            .unwrap_err()
            .to_string()
            .contains("account does not exist"));
        assert_eq!(
            store
                .highest_mailbox_uid(missing_account_id, "INBOX")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn notification_baseline_never_reports_non_inbox_mail() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .save_synced_messages(account_id, "Archive", &[])
            .await
            .unwrap();
        let mut archived = message("Archived", "Not a new-mail alert");
        archived.account_id = account_id.to_string();
        archived.mailbox = "Archive".into();

        assert!(store
            .save_synced_messages(account_id, "Archive", &[archived])
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn complete_hydration_marks_transient_content_without_duplicate_rows() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut headers = message("Fast arrival", "");
        headers.account_id = account_id.to_string();
        headers.id = stable_message_id(account_id, "INBOX", 1);
        headers.content_state = "headers_only".into();
        store
            .save_synced_messages(account_id, "INBOX", &[headers.clone()])
            .await
            .unwrap();
        assert!(store.claim_message_hydration(&headers.id).await.unwrap());
        assert!(!store.claim_message_hydration(&headers.id).await.unwrap());

        let mut complete = headers;
        complete.body_text = "Downloaded body".into();
        complete.snippet = "Downloaded body".into();
        complete.content_state = "complete".into();
        store.upsert_messages(&[complete.clone()]).await.unwrap();

        let stored = store.message(&complete.id).await.unwrap().unwrap();
        assert_eq!(stored.content_state, "complete");
        assert!(stored.body_text.is_empty());
    }

    #[tokio::test]
    async fn returns_the_highest_uid_for_a_mailbox() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut first = message("First", "one");
        first.account_id = account_id.to_string();
        first.uid = 3;
        let mut newest = message("Newest", "two");
        newest.account_id = account_id.to_string();
        newest.uid = 9;
        store.upsert_messages(&[first, newest]).await.unwrap();

        assert_eq!(
            store
                .highest_mailbox_uid(account_id, "INBOX")
                .await
                .unwrap(),
            Some(9)
        );
        assert_eq!(
            store
                .highest_mailbox_uid(account_id, "Archive")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn generation_guard_rejects_provider_publication_after_the_last_precheck() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let generation = store.account_search_generation(account_id).await.unwrap();

        // Model the narrow race: a provider worker has captured the current
        // generation, then a foreground mutation becomes authoritative just
        // before it attempts its catalogue/cache transaction.
        store
            .advance_account_search_generation(account_id)
            .await
            .unwrap();
        let mut stale = message("stale provider subject", "stale provider body");
        stale.id = "stale-provider-publication".into();
        stale.account_id = account_id.to_string();
        assert!(!store
            .upsert_catalog_messages_if_account_generation(
                account_id,
                generation,
                std::slice::from_ref(&stale),
            )
            .await
            .unwrap());
        assert!(store.message(&stale.id).await.unwrap().is_none());

        let mailbox_draft = SelectableMailboxDraft {
            remote_path: "INBOX".into(),
            local_path: Some("INBOX".into()),
            hierarchy_delimiter: Some("/".into()),
            parent_id: None,
            parent_path: None,
            special_use: Some("inbox".into()),
            selectable: true,
            uid_validity: Some(77),
            catalogue_coverage: "partial".into(),
        };
        assert!(
            store
                .upsert_selectable_mailbox_if_account_generation(
                    account_id,
                    generation,
                    &mailbox_draft,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(store
            .selectable_mailbox_catalogue(account_id)
            .await
            .unwrap()
            .is_empty());
        assert!(!store
            .save_mailbox_catalog_state_if_account_generation(
                account_id, generation, "INBOX", "INBOX", 77, 1, false,
            )
            .await
            .unwrap());
        assert!(store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .is_none());

        store
            .upsert_catalog_messages(&[stale.clone()])
            .await
            .unwrap();
        let live_mailbox = store
            .upsert_selectable_mailbox(account_id, &mailbox_draft)
            .await
            .unwrap();
        assert!(!store
            .set_message_mailbox_memberships_if_account_generation(
                account_id,
                generation,
                &stale.id,
                std::slice::from_ref(&live_mailbox.id),
            )
            .await
            .unwrap());
        assert!(store
            .list_message_mailbox_memberships(account_id, &stale.id)
            .await
            .unwrap()
            .is_empty());
        assert!(!store
            .cache_search_body_text_if_account_generation(
                account_id,
                generation,
                &stale.id,
                "stale cache body",
            )
            .await
            .unwrap());
        assert!(store
            .cached_search_body_text(&stale.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn account_generation_advances_with_authoritative_account_reset_and_flag_changes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut account = account_with_id(account_id, "generation@example.test");
        store.save_account(&account).await.unwrap();
        assert_eq!(
            store.account_search_generation(account_id).await.unwrap(),
            0
        );

        account.display_name = "Updated generation test".into();
        store.save_account(&account).await.unwrap();
        assert_eq!(
            store.account_search_generation(account_id).await.unwrap(),
            1
        );

        let mut row = message("generation flags", "body");
        row.id = "generation-flag-row".into();
        row.account_id = account_id.to_string();
        store.upsert_catalog_messages(&[row.clone()]).await.unwrap();
        store.set_message_read(&row.id, true).await.unwrap();
        assert_eq!(
            store.account_search_generation(account_id).await.unwrap(),
            2
        );
        store.set_message_flagged(&row.id, true).await.unwrap();
        assert_eq!(
            store.account_search_generation(account_id).await.unwrap(),
            3
        );
        store.reset_account_mail_index(account_id).await.unwrap();
        assert_eq!(
            store.account_search_generation(account_id).await.unwrap(),
            4
        );
    }

    #[tokio::test]
    async fn local_flag_repairs_allow_legacy_orphans_but_respect_deleted_account_fences() {
        let store = Store::in_memory().await.unwrap();
        let orphan_account = uuid::Uuid::new_v4();
        let mut row = message("legacy orphan", "body");
        row.id = "legacy-orphan-flag-row".into();
        row.account_id = orphan_account.to_string();
        store.upsert_catalog_messages(&[row.clone()]).await.unwrap();

        // Profiles upgraded from older releases can have catalogue rows whose
        // account data was already cleaned up. Local maintenance remains safe
        // and must not invent a generation/provider identity for that row.
        store.set_message_read(&row.id, true).await.unwrap();
        assert!(store.message(&row.id).await.unwrap().unwrap().is_read);
        assert!(!sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM account_search_generations WHERE account_id = ?)"
        )
        .bind(orphan_account.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap());

        sqlx::query("INSERT INTO deleted_account_tombstones(account_id, deleted_at) VALUES (?, ?)")
            .bind(orphan_account.to_string())
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.set_message_flagged(&row.id, true).await.is_err());
        assert!(!store.message(&row.id).await.unwrap().unwrap().is_flagged);
    }

    #[tokio::test]
    async fn catalogue_state_reconciles_deletions_flags_and_uidvalidity() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut first = message("First", "one");
        first.account_id = account_id.to_string();
        first.uid = 1;
        let mut second = message("Second", "two");
        second.account_id = account_id.to_string();
        second.uid = 2;
        store
            .upsert_catalog_messages(&[first, second])
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 77, 2, false)
            .await
            .unwrap();
        store
            .update_mailbox_flags(account_id, "INBOX", &[(2, true, true)])
            .await
            .unwrap();
        let remote = [2].into_iter().collect();
        assert_eq!(
            store
                .reconcile_mailbox_uids(account_id, "INBOX", &remote)
                .await
                .unwrap(),
            1
        );
        let remaining = store
            .message_by_locator(account_id, "INBOX", 2)
            .await
            .unwrap()
            .unwrap();
        assert!(remaining.is_read && remaining.is_flagged);
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.uid_validity, 77);
        assert!(!state.historical_complete);
    }

    #[tokio::test]
    async fn catalogue_handles_fifty_thousand_metadata_only_messages() {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut base = message("Catalogue row", "preview only");
        base.account_id = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=50_000_i64 {
            let mut row = base.clone();
            row.id = format!("catalogue-{uid}");
            row.thread_id = row.id.clone();
            row.uid = uid;
            if uid == 49_999 {
                row.subject = "Needle in a large catalogue".into();
            }
            persist_message(&mut tx, &row).await.unwrap();
        }
        tx.commit().await.unwrap();

        let (count, body_bytes): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(length(body_text)), 0) FROM messages")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(count, 50_000);
        assert_eq!(body_bytes, 0);
        // Time only the hot query path, not the deliberately large fixture
        // transaction above. The SQL compiler should narrow the indexed
        // subject candidate before canonical evaluation and pagination.
        let search_started = Instant::now();
        let matches = store
            .search(&SearchQuery {
                text: "needle".into(),
                account_ids: vec![account_id],
                limit: Some(25),
                ..SearchQuery::default()
            })
            .await
            .unwrap();
        assert!(
            search_started.elapsed() < Duration::from_millis(250),
            "first local search page exceeded the 250 ms target"
        );
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn broad_local_conversation_searches_stop_after_a_bounded_candidate_page() {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut base = message("Plain catalogue row", "preview only");
        base.account_id = account_id.to_string();
        base.mailbox = "Projects/2026".into();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=50_000_i64 {
            let mut row = base.clone();
            row.id = format!("broad-local-{uid:05}");
            row.thread_id = row.id.clone();
            row.uid = uid;
            row.received_at = Utc::now() + chrono::Duration::seconds(uid);
            persist_message(&mut tx, &row).await.unwrap();
        }
        tx.commit().await.unwrap();

        for text in [
            // A partial NOT/OR candidate must remain canonical, but the
            // first page still needs only its 26 representatives.
            "NOT has:noattachment OR subject:plain",
            "has:noattachment",
            "in:Projects/*",
        ] {
            let started = Instant::now();
            let page = store
                .search_conversation_page(&SearchQuery {
                    text: text.into(),
                    account_ids: vec![account_id],
                    limit: Some(25),
                    ..SearchQuery::default()
                })
                .await
                .unwrap();
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "{text} first page exceeded the 250 ms target: {:?}",
                started.elapsed()
            );
            assert_eq!(page.conversations.len(), 25, "{text}");
            assert!(page.next_cursor.is_some(), "{text}");
            assert!(page.candidate_cursor.is_some(), "{text}");
        }
    }

    #[tokio::test]
    async fn sparse_broad_search_returns_a_continuation_without_scanning_the_catalogue() {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut base = message("ordinary", "preview only");
        base.account_id = account_id.to_string();
        base.mailbox = "Projects/2026".into();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=50_000_i64 {
            let mut row = base.clone();
            row.id = format!("sparse-local-{uid:05}");
            row.thread_id = row.id.clone();
            row.uid = uid;
            row.received_at = Utc::now() + chrono::Duration::seconds(uid);
            if uid == 49_300 {
                row.subject = "rare needle".into();
            }
            persist_message(&mut tx, &row).await.unwrap();
        }
        tx.commit().await.unwrap();
        // `has:noattachment` must broaden to canonical evaluation. The NOT
        // branch leaves only the rare subject match, which is outside the
        // first candidate slice.
        let query = SearchQuery {
            text: "NOT has:noattachment OR subject:rare".into(),
            account_ids: vec![account_id],
            limit: Some(25),
            ..SearchQuery::default()
        };
        let started = Instant::now();
        let first = store.search_conversation_page(&query).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "sparse broad first page exceeded the 250 ms target: {:?}",
            started.elapsed()
        );
        assert!(first.conversations.is_empty());
        assert!(first.next_cursor.is_none());
        assert!(first.candidate_cursor.is_some());
        assert!(!first.candidate_exhausted);

        // Candidate continuation resumes exactly after the bounded first
        // slice. It eventually reaches the rare row without skipping it.
        let second = store
            .search_conversation_page_from_candidate(&query, first.candidate_cursor.as_ref(), &[])
            .await
            .unwrap();
        assert!(second.conversations.is_empty());
        assert!(second.candidate_cursor.is_some());
        let third = store
            .search_conversation_page_from_candidate(&query, second.candidate_cursor.as_ref(), &[])
            .await
            .unwrap();
        assert_eq!(third.conversations.len(), 1);
        assert_eq!(third.conversations[0].latest.subject, "rare needle");
    }

    #[tokio::test]
    async fn candidate_cursor_keeps_older_thread_matches_from_reappearing() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut rows = Vec::new();
        for (thread, seconds) in [("one", 3_i64), ("two", 2_i64), ("three", 1_i64)] {
            for ordinal in 0..2_i64 {
                let mut row = message("needle", "body");
                row.id = format!("{thread}-{ordinal}");
                row.thread_id = thread.into();
                row.account_id = account_id.to_string();
                row.mailbox = "Projects/2026".into();
                row.uid = rows.len() as i64 + 1;
                row.received_at = Utc::now() + chrono::Duration::seconds(seconds * 10 + ordinal);
                rows.push(row);
            }
        }
        store.upsert_messages(&rows).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            6
        );
        let query = SearchQuery {
            text: "in:Projects/* needle".into(),
            account_ids: vec![account_id],
            limit: Some(1),
            ..SearchQuery::default()
        };
        let first = store
            .search_conversation_page_from_candidate(&query, None, &[])
            .await
            .unwrap();
        let first_id = first.conversations[0].id.clone();
        let second = store
            .search_conversation_page_from_candidate(
                &query,
                first.candidate_cursor.as_ref(),
                std::slice::from_ref(&first_id),
            )
            .await
            .unwrap();
        assert_ne!(second.conversations[0].id, first_id);
        let mut all_matches_query = query.clone();
        all_matches_query.limit = Some(100);
        assert_eq!(store.search(&all_matches_query).await.unwrap().len(), 6);
        assert_eq!(second.match_evidence.len(), 1);
    }

    #[tokio::test]
    async fn search_v2_migration_restarts_in_bounded_batches_for_a_large_legacy_catalogue() {
        let _large_dataset = crate::large_dataset_test_guard().await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut base = message("Legacy catalogue row", "preview");
        base.account_id = account_id.to_string();
        let mut tx = store.pool.begin().await.unwrap();
        for uid in 1..=50_000_i64 {
            let mut row = base.clone();
            row.id = format!("legacy-v2-{uid}");
            row.thread_id = row.id.clone();
            row.uid = uid;
            persist_message(&mut tx, &row).await.unwrap();
        }
        tx.commit().await.unwrap();
        for statement in [
            "DROP TRIGGER IF EXISTS messages_fts_v2_ai",
            "DROP TRIGGER IF EXISTS messages_fts_v2_ad",
            "DROP TRIGGER IF EXISTS messages_fts_v2_au",
            "DROP TABLE IF EXISTS messages_fts_v2",
            "DELETE FROM search_catalogue_v2_progress",
            "DELETE FROM app_meta WHERE key = 'search_catalogue_v2'",
        ] {
            sqlx::query(statement).execute(&store.pool).await.unwrap();
        }
        assert!(sqlx::query_scalar::<_, Option<String>>(
            "SELECT value FROM app_meta WHERE key = 'search_catalogue_v2'",
        )
        .fetch_optional(&store.pool)
        .await
        .unwrap()
        .flatten()
        .is_none());
        store.pool.close().await;
        drop(store);

        let first_open = Instant::now();
        let store = Store::open(&database).await.unwrap();
        assert!(
            first_open.elapsed() < Duration::from_secs(2),
            "a restart-safe migration open must not rebuild all 50,000 rows"
        );
        let reopened_messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(reopened_messages, 50_000);
        let first_progress: (i64, i64, bool) = sqlx::query_as(
            "SELECT last_rowid, target_rowid, complete FROM search_catalogue_v2_progress WHERE stage = 'headers'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            first_progress.0, SEARCH_CATALOGUE_V2_MIGRATION_BATCH_SIZE,
            "unexpected first progress: {first_progress:?}"
        );
        assert_eq!(first_progress.1, 50_000);
        assert!(!first_progress.2);
        assert!(!store.search_catalogue_v2_complete().await.unwrap());
        // Exercise live writes on both sides of the first resumable batch.
        // `legacy-v2-400` has an FTS row already; `legacy-v2-501` does not.
        // The old external-content trigger failed the latter update/delete or
        // made the later backfill insert collide with it.
        sqlx::query("UPDATE messages SET subject = 'updated during resumable migration' WHERE id = 'legacy-v2-400'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET subject = 'also updated before its batch' WHERE id = 'legacy-v2-501'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM messages WHERE id IN ('legacy-v2-500', 'legacy-v2-502')")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        drop(store);

        let restart = Instant::now();
        let store = Store::open(&database).await.unwrap();
        assert!(restart.elapsed() < Duration::from_secs(2));
        let second_last_rowid: i64 = sqlx::query_scalar(
            "SELECT last_rowid FROM search_catalogue_v2_progress WHERE stage = 'headers'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            second_last_rowid,
            SEARCH_CATALOGUE_V2_MIGRATION_BATCH_SIZE * 2 + 1,
            "the deleted row is skipped while the next live row still fills the bounded batch"
        );
        let mut progress = store.search_catalogue_v2_backfill_progress().await.unwrap();
        while !progress.complete {
            progress = store.advance_search_catalogue_v2_backfill().await.unwrap();
        }
        // Progress is a durable rowid high-water mark captured before the
        // concurrent deletes. The index itself must reflect the live row set.
        assert_eq!(progress.indexed_messages, 50_000);
        assert_eq!(progress.total_messages, 50_000);
        let indexed_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages_fts_v2")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            indexed_rows, 49_998,
            "each live row has exactly one FTS document"
        );
        let updated_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages_fts_v2 WHERE messages_fts_v2 MATCH 'updated'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            updated_rows, 2,
            "both pre- and post-batch live updates survive"
        );
        store.pool.close().await;
        drop(store);

        let store = Store::open(&database).await.unwrap();
        assert!(
            store
                .search_catalogue_v2_backfill_progress()
                .await
                .unwrap()
                .complete,
            "completed progress survives restart"
        );
    }

    #[tokio::test]
    async fn identifies_only_legacy_invented_message_dates_for_refetch() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut assumed = message("Assumed date", "Needs repair");
        assumed.account_id = account_id.to_string();
        assumed.uid = 7;
        assumed.received_at = "2026-07-19T17:57:56.561510Z".parse().unwrap();
        let mut real = message("Real date", "Leave alone");
        real.account_id = account_id.to_string();
        real.uid = 8;
        real.received_at = "2026-02-09T06:16:47Z".parse().unwrap();
        store.upsert_messages(&[assumed, real]).await.unwrap();

        assert_eq!(
            store
                .legacy_assumed_date_uids(account_id, "INBOX", 10)
                .await
                .unwrap(),
            vec![7]
        );
    }

    #[tokio::test]
    async fn identifies_only_mime_encoded_html_snippets_for_refetch() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut encoded = message("Encoded preview", "");
        encoded.account_id = account_id.to_string();
        encoded.uid = 7;
        encoded.snippet = "PHA+WW91IGhhdmUgYSBtZXNzYWdlPC9wPg==".into();
        let mut normal = message("Normal preview", "");
        normal.account_id = account_id.to_string();
        normal.uid = 8;
        normal.snippet = "You have a message".into();
        store.upsert_messages(&[encoded, normal]).await.unwrap();

        assert_eq!(
            store
                .mime_encoded_snippet_uids(account_id, "INBOX")
                .await
                .unwrap(),
            vec![7]
        );
    }

    #[tokio::test]
    async fn identifies_and_stops_refetching_misclassified_inline_bodies() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut broken = message("Sent reply", "");
        broken.id = stable_message_id(account_id, "Sent", 12);
        broken.account_id = account_id.to_string();
        broken.mailbox = "Sent".into();
        broken.uid = 12;
        broken.has_attachments = true;
        broken.attachments = vec![AttachmentData {
            attachment: Attachment {
                id: format!("{}:0", broken.id),
                message_id: broken.id.clone(),
                filename: "attachment".into(),
                mime_type: "text/plain".into(),
                size_bytes: 10,
                is_inline: true,
                presentation: AttachmentPresentation::Unknown,
                is_potentially_unsafe: false,
            },
            bytes: b"Sent reply".to_vec(),
        }];
        store.upsert_messages(&[broken.clone()]).await.unwrap();
        let legacy_attachment = &broken.attachments[0];
        sqlx::query("INSERT INTO attachments(id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&legacy_attachment.attachment.id)
            .bind(&broken.id)
            .bind(&legacy_attachment.attachment.filename)
            .bind(&legacy_attachment.attachment.mime_type)
            .bind(legacy_attachment.attachment.size_bytes)
            .bind(legacy_attachment.attachment.is_inline)
            .bind(legacy_attachment.attachment.is_potentially_unsafe)
            .bind(&legacy_attachment.bytes)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(
            store
                .misclassified_body_uids(account_id, "Sent", 10)
                .await
                .unwrap(),
            vec![12]
        );

        broken.body_text = "Sent reply".into();
        broken.body_html = Some("<p>Sent reply</p>".into());
        broken.has_attachments = false;
        broken.attachments.clear();
        store.upsert_messages(&[broken]).await.unwrap();

        assert!(store
            .misclassified_body_uids(account_id, "Sent", 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn encrypted_secrets_survive_reopening_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.unwrap();

        store
            .set_secret("mail:account-1", "correct horse battery staple")
            .await
            .unwrap();
        let (_, ciphertext): (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT nonce, ciphertext FROM credentials WHERE name = ?")
                .bind("mail:account-1")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(!String::from_utf8_lossy(&ciphertext).contains("correct horse"));
        assert_eq!(
            store.secret("mail:account-1").await.unwrap().as_deref(),
            Some("correct horse battery staple")
        );

        store.pool.close().await;
        drop(store);
        let reopened = Store::open(&database).await.unwrap();
        assert_eq!(
            reopened.secret("mail:account-1").await.unwrap().as_deref(),
            Some("correct horse battery staple")
        );
        assert_eq!(
            std::fs::read(directory.path().join(VAULT_KEY_FILE))
                .unwrap()
                .len(),
            VAULT_KEY_LEN
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::metadata(directory.path().join(VAULT_KEY_FILE))
                .unwrap()
                .permissions();
            assert_eq!(permissions.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn vault_key_publication_falls_back_without_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join(".vault.key.test.tmp");
        let canonical = directory.path().join(VAULT_KEY_FILE);
        let key = [7_u8; VAULT_KEY_LEN];
        std::fs::write(&temporary, key).unwrap();

        publish_vault_key(&temporary, &canonical, false).unwrap();
        assert_eq!(std::fs::read(&canonical).unwrap(), key);
        assert!(!temporary.exists());

        let contender = directory.path().join(".vault.key.contender.tmp");
        std::fs::write(&contender, [9_u8; VAULT_KEY_LEN]).unwrap();
        assert_eq!(
            publish_vault_key(&contender, &canonical, false)
                .unwrap_err()
                .kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), key);

        std::fs::remove_file(&canonical).unwrap();
        let abandoned_lock = vault_publish_lock_path(&canonical);
        std::fs::write(&abandoned_lock, "crashed-publisher").unwrap();
        let recovered = directory.path().join(".vault.key.recovered.tmp");
        std::fs::write(&recovered, [8_u8; VAULT_KEY_LEN]).unwrap();
        publish_vault_key_with_stale_after(&recovered, &canonical, false, Duration::ZERO).unwrap();
        assert_eq!(std::fs::read(&canonical).unwrap(), [8_u8; VAULT_KEY_LEN]);
        assert!(!abandoned_lock.exists());
    }

    #[test]
    fn slow_vault_publisher_heartbeats_while_a_contender_waits() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join(VAULT_KEY_FILE);
        let holder_temporary = directory.path().join(".vault.key.holder.tmp");
        let contender_temporary = directory.path().join(".vault.key.contender.tmp");
        std::fs::write(&holder_temporary, [3_u8; VAULT_KEY_LEN]).unwrap();
        std::fs::write(&contender_temporary, [4_u8; VAULT_KEY_LEN]).unwrap();
        let (holder_ready_tx, holder_ready_rx) = std::sync::mpsc::channel();
        let (finish_holder_tx, finish_holder_rx) = std::sync::mpsc::channel();
        let holder_path = holder_temporary.clone();
        let canonical_path = canonical.clone();

        let holder = std::thread::spawn(move || {
            publish_vault_key_with_stale_after_and_hook(
                &holder_path,
                &canonical_path,
                false,
                Duration::from_millis(40),
                || {
                    holder_ready_tx.send(()).unwrap();
                    finish_holder_rx.recv().unwrap();
                },
            )
        });
        holder_ready_rx.recv().unwrap();
        // Wait beyond the stale budget: without the heartbeat, the contender
        // would remove the live holder's lock and publish its own key.
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            publish_vault_key_with_stale_after(
                &contender_temporary,
                &canonical,
                false,
                Duration::from_millis(40),
            )
            .unwrap_err()
            .kind(),
            ErrorKind::TimedOut
        );
        assert!(!canonical.exists());
        finish_holder_tx.send(()).unwrap();
        holder.join().unwrap().unwrap();
        assert_eq!(std::fs::read(&canonical).unwrap(), [3_u8; VAULT_KEY_LEN]);
        assert!(contender_temporary.exists());
        assert!(!vault_publish_lock_path(&canonical).exists());
    }

    #[test]
    fn vault_publisher_that_loses_its_lock_cannot_publish_or_remove_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join(VAULT_KEY_FILE);
        let temporary = directory.path().join(".vault.key.displaced.tmp");
        std::fs::write(&temporary, [5_u8; VAULT_KEY_LEN]).unwrap();
        let replacement_lock = vault_publish_lock_path(&canonical);

        let error = publish_vault_key_with_stale_after_and_hook(
            &temporary,
            &canonical,
            false,
            Duration::from_secs(1),
            || {
                std::fs::remove_file(&replacement_lock).unwrap();
                std::fs::write(&replacement_lock, "new-owner-token").unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(!canonical.exists());
        assert!(temporary.exists());
        assert_eq!(
            std::fs::read_to_string(&replacement_lock).unwrap(),
            "new-owner-token"
        );
    }

    #[tokio::test]
    async fn encrypted_secrets_detect_tampering_and_can_be_deleted() {
        let store = Store::in_memory().await.unwrap();
        store.set_secret("mail:account-1", "secret").await.unwrap();
        sqlx::query("UPDATE credentials SET ciphertext = ? WHERE name = ?")
            .bind(vec![0_u8; 32])
            .bind("mail:account-1")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.secret("mail:account-1").await.is_err());

        store.delete_secret("mail:account-1").await.unwrap();
        assert!(store.secret("mail:account-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_an_account_also_deletes_its_local_messages() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "hello@dakia.dev".into(),
            display_name: "Dakia".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        let mut stored_message = message("Account removal", "Remove this local copy");
        stored_message.account_id = account.id.to_string();
        store
            .upsert_messages(std::slice::from_ref(&stored_message))
            .await
            .unwrap();

        store.delete_account(account.id).await.unwrap();

        assert!(store.account(account.id).await.unwrap().is_none());
        assert!(store
            .search(&SearchQuery {
                account_ids: vec![account.id],
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn deleted_account_fence_rejects_late_provider_publication() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "late-write@dakia.dev".into(),
            display_name: "Late write".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        store.delete_account(account.id).await.unwrap();

        let mut late_message = message("Late IMAP write", "must not return");
        late_message.id = stable_message_id(account.id, "INBOX", 91);
        late_message.account_id = account.id.to_string();
        late_message.uid = 91;
        let failures = [
            store
                .upsert_messages(std::slice::from_ref(&late_message))
                .await
                .err(),
            store
                .upsert_catalog_messages(std::slice::from_ref(&late_message))
                .await
                .err(),
            store
                .save_synced_messages(account.id, "INBOX", &[late_message.clone()])
                .await
                .err(),
            store
                .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, false)
                .await
                .err(),
            store
                .save_mail_rebuild_job(&MailRebuildJob {
                    account_id: account.id,
                    phase: "downloading".into(),
                    completed: 1,
                    total: Some(2),
                    reset_before_sync: false,
                })
                .await
                .err(),
            store
                .move_message(account.id, "INBOX", 91, "Archive", None)
                .await
                .err(),
        ];
        assert!(failures.iter().all(|failure| failure
            .as_ref()
            .is_some_and(|error| error.to_string().contains("account was removed"))));

        for table in [
            "messages",
            "mailbox_sync_state",
            "mailbox_catalog_state",
            "mail_rebuild_jobs",
            "mailbox_action_tombstones",
            "sent_correspondents",
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ?"
            ))
            .bind(account.id.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(count, 0, "late write reached {table}");
        }

        // A delayed Settings save cannot clear the fence and recreate the
        // deleted UUID. A deliberate reconnect creates a fresh account ID.
        assert!(store.save_account(&account).await.is_err());
        assert!(store.account(account.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reopening_cleans_pre_guard_orphaned_account_state() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("orphan-repair.db");
        let initial = Store::open(&database).await.unwrap();
        let account = AccountDraft {
            email: "survives-repair@dakia.dev".into(),
            display_name: "Survives repair".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        initial.save_account(&account).await.unwrap();
        let mut valid = message("Keep this message", "valid state");
        valid.id = stable_message_id(account.id, "INBOX", 1);
        valid.account_id = account.id.to_string();
        initial
            .upsert_messages(std::slice::from_ref(&valid))
            .await
            .unwrap();
        initial
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, true)
            .await
            .unwrap();

        let orphan_id = uuid::Uuid::new_v4();
        let mut orphan = message("Removed account message", "orphaned state");
        orphan.id = stable_message_id(orphan_id, "INBOX", 41);
        orphan.account_id = orphan_id.to_string();
        orphan.uid = 41;
        orphan.is_flagged = true;
        initial
            .upsert_messages(std::slice::from_ref(&orphan))
            .await
            .unwrap();
        // Simulate state created by a pre-guard build. The public sync path
        // now rejects a missing account, so legacy-orphan repair is exercised
        // by inserting the old durable row directly.
        sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid) VALUES (?, 'INBOX', ?, 41)")
            .bind(orphan_id.to_string())
            .bind(Utc::now())
            .execute(&initial.pool)
            .await
            .unwrap();
        initial
            .save_mailbox_catalog_state(orphan_id, "INBOX", "INBOX", 9, 1, false)
            .await
            .unwrap();
        initial
            .save_mail_rebuild_job(&MailRebuildJob {
                account_id: orphan_id,
                phase: "downloading".into(),
                completed: 1,
                total: Some(3),
                reset_before_sync: false,
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'INBOX', 41, ?)")
            .bind(orphan_id.to_string())
            .bind(Utc::now())
            .execute(&initial.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sent_correspondents(account_id, address) VALUES (?, 'orphan@example.test')")
            .bind(orphan_id.to_string())
            .execute(&initial.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO attachments(id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe, data) VALUES ('orphan-attachment', ?, 'old.txt', 'text/plain', 3, 0, 0, X'6f6c64')")
            .bind(&orphan.id)
            .execute(&initial.pool)
            .await
            .unwrap();
        sqlx::query("INSERT OR REPLACE INTO starred_message_bodies(message_id, body_text, body_html, cached_at) VALUES (?, 'old', NULL, ?)")
            .bind(&orphan.id)
            .bind(Utc::now())
            .execute(&initial.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO starred_attachment_metadata(id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe) VALUES ('orphan-attachment-meta', ?, 'old.txt', 'text/plain', 3, 0, 0)")
            .bind(&orphan.id)
            .execute(&initial.pool)
            .await
            .unwrap();
        drop(initial);

        let repaired = Store::open(&database).await.unwrap();
        assert!(repaired.account(account.id).await.unwrap().is_some());
        assert!(repaired
            .message_by_locator(account.id, "INBOX", 1)
            .await
            .unwrap()
            .is_some());
        assert!(repaired
            .mailbox_catalog_state(account.id, "INBOX")
            .await
            .unwrap()
            .is_some());

        for table in [
            "messages",
            "mailbox_sync_state",
            "mailbox_catalog_state",
            "mail_rebuild_jobs",
            "mailbox_action_tombstones",
            "sent_correspondents",
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ?"
            ))
            .bind(orphan_id.to_string())
            .fetch_one(&repaired.pool)
            .await
            .unwrap();
            assert_eq!(count, 0, "orphan row remained in {table}");
        }
        for table in [
            "attachments",
            "starred_message_bodies",
            "starred_attachment_metadata",
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE message_id = ?"
            ))
            .bind(&orphan.id)
            .fetch_one(&repaired.pool)
            .await
            .unwrap();
            assert_eq!(count, 0, "orphan dependent remained in {table}");
        }
        assert!(repaired
            .save_mail_rebuild_job(&MailRebuildJob {
                account_id: orphan_id,
                phase: "late".into(),
                completed: 0,
                total: None,
                reset_before_sync: false,
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resetting_an_account_index_preserves_account_and_credentials() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "rebuild@dakia.dev".into(),
            display_name: "Rebuild".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        let other = AccountDraft {
            email: "other@dakia.dev".into(),
            display_name: "Other".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        store.save_account(&other).await.unwrap();
        store
            .set_secret("mail:rebuild", "still-secret")
            .await
            .unwrap();

        let mut indexed = message("Broken preview", "--boundary Content-Type: text/plain");
        indexed.account_id = account.id.to_string();
        indexed.is_flagged = true;
        let indexed_id = indexed.id.clone();
        let mut untouched = message("Other account", "Keep this row");
        untouched.id = "other-message".into();
        untouched.account_id = other.id.to_string();
        store.upsert_messages(&[indexed, untouched]).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, true)
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account.id, "INBOX", Some(7))
            .await
            .unwrap();

        store.reset_account_mail_index(account.id).await.unwrap();

        assert!(store.account(account.id).await.unwrap().is_some());
        assert_eq!(
            store.secret("mail:rebuild").await.unwrap().as_deref(),
            Some("still-secret")
        );
        assert!(store
            .search(&SearchQuery {
                account_ids: vec![account.id],
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .mailbox_catalog_state(account.id, "INBOX")
            .await
            .unwrap()
            .is_none());
        assert!(store.starred_body(&indexed_id).await.unwrap().is_none());
        assert_eq!(
            store
                .search(&SearchQuery {
                    account_ids: vec![other.id],
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn starred_messages_cache_bodies_without_attachment_bytes() {
        let store = Store::in_memory().await.unwrap();
        let mut starred = message("Keep offline", "Offline body");
        starred.is_flagged = true;
        starred.attachments.push(AttachmentData {
            attachment: Attachment {
                id: "attachment-1".into(),
                message_id: starred.id.clone(),
                filename: "notes.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 4,
                is_inline: false,
                presentation: AttachmentPresentation::Downloadable,
                is_potentially_unsafe: false,
            },
            bytes: vec![1, 2, 3, 4],
        });
        let id = starred.id.clone();
        store.upsert_messages(&[starred]).await.unwrap();

        assert_eq!(
            store.starred_body(&id).await.unwrap().unwrap().0,
            "Offline body"
        );
        assert_eq!(
            store.starred_attachment_metadata(&id).await.unwrap().len(),
            1
        );
        let stored_bytes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE message_id = ?")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(stored_bytes, 0);

        store.set_message_flagged(&id, false).await.unwrap();
        assert!(store.starred_body(&id).await.unwrap().is_none());
        assert!(store
            .starred_attachment_metadata(&id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn starred_count_deduplicates_conversations() {
        let store = Store::in_memory().await.unwrap();
        let mut first = message("First", "One");
        first.is_flagged = true;
        let account_id = uuid::Uuid::parse_str(&first.account_id).unwrap();
        let mut second = message("Second", "Two");
        second.account_id = first.account_id.clone();
        second.thread_id = first.thread_id.clone();
        second.uid = 2;
        second.is_flagged = true;
        let shared_thread_id = first.thread_id.clone();
        store.upsert_messages(&[first, second]).await.unwrap();
        sqlx::query("UPDATE messages SET thread_id = ? WHERE account_id = ?")
            .bind(shared_thread_id)
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(
            store
                .starred_conversation_count(&[account_id])
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn set_message_read_updates_the_catalogue_row() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Unread", "Reader");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();

        store.set_message_read(&id, true).await.unwrap();
        assert!(store.message(&id).await.unwrap().unwrap().is_read);

        store.set_message_read(&id, false).await.unwrap();
        assert!(!store.message(&id).await.unwrap().unwrap().is_read);
    }

    #[tokio::test]
    async fn legacy_accounts_default_their_local_name_to_email() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "legacy@dakia.dev".into(),
            display_name: "Dakia".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        let mut legacy = serde_json::to_value(&account).unwrap();
        legacy.as_object_mut().unwrap().remove("account_name");
        sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?)")
            .bind(account.id.to_string())
            .bind(&account.email)
            .bind(legacy.to_string())
            .bind(account.created_at)
            .execute(&store.pool)
            .await
            .unwrap();

        let restored = store.account(account.id).await.unwrap().unwrap();
        assert_eq!(restored.account_name, restored.email);
    }

    #[tokio::test]
    async fn looks_up_all_catalogue_states_by_provider_mailbox_case_insensitively() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        store
            .save_mailbox_catalog_state(account_id, "Archive", "[Gmail]/All Mail", 7, 0, true)
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "[Gmail]/All Mail", 7, 0, true)
            .await
            .unwrap();

        let states = store
            .mailbox_catalog_states_for_remote(account_id, "[gmail]/all mail")
            .await
            .unwrap();
        assert_eq!(
            states
                .iter()
                .map(|state| state.mailbox.as_str())
                .collect::<Vec<_>>(),
            vec!["INBOX", "Archive"]
        );
        assert!(states.iter().all(|state| state.uid_validity == 7));
        assert!(store
            .mailbox_catalog_states_for_remote(account_id, "Projects")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn synced_message_above_a_gap_does_not_advance_the_contiguous_watermark() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "budget@example.test".into(),
            display_name: "Budget".into(),
            provider_id: Some("fastmail".into()),
            username: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();

        let mut higher = message("Higher UID", "available before the gap is repaired");
        higher.account_id = account.id.to_string();
        higher.uid = 42;
        higher.id = stable_message_id(account.id, "INBOX", 42);
        store
            .save_synced_messages_through(account.id, "INBOX", &[higher], Some(41))
            .await
            .unwrap();

        assert_eq!(
            store
                .highest_mailbox_uid(account.id, "INBOX")
                .await
                .unwrap(),
            Some(41)
        );
        assert_eq!(
            store.mailbox_uids(account.id, "INBOX").await.unwrap(),
            [42].into()
        );
    }

    #[tokio::test]
    async fn uidvalidity_change_preserves_identity_and_rows_until_replacement_finalizes() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("uidvalidity-retry.db");
        let store = Store::open(&database).await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old UID namespace", "old");
        old.account_id = account_id.to_string();
        let old_id = old.id.clone();
        store
            .save_synced_messages(account_id, "INBOX", &[old])
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account_id, "INBOX", Some(10))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();
        sqlx::query("UPDATE mailbox_sync_state SET initialized_at = ?, highest_uid = 2, uid_validity = 10 WHERE account_id = ? AND mailbox = 'INBOX'")
            .bind(Utc::now())
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        let reset = store
            .prepare_mailbox_sync(account_id, "INBOX", Some(11))
            .await
            .unwrap();
        assert!(!reset.initialized);
        assert_eq!(reset.highest_uid, None);
        assert_eq!(reset.uid_validity, Some(11));
        assert!(reset.uid_validity_changed);
        let old_catalogue = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_catalogue.uid_validity, 10);
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 1)
                .await
                .unwrap()
                .as_ref()
                .map(|message| &message.id),
            Some(&old_id),
            "UIDVALIDITY rollover must not delete committed mail before a replacement snapshot finalizes"
        );
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        let retry = reopened
            .prepare_mailbox_sync(account_id, "INBOX", Some(11))
            .await
            .unwrap();
        assert!(!retry.initialized);
        assert_eq!(retry.highest_uid, None);
        assert!(retry.uid_validity_changed);
        assert_eq!(
            reopened
                .mailbox_catalog_state(account_id, "INBOX")
                .await
                .unwrap()
                .unwrap()
                .uid_validity,
            10
        );
    }

    #[tokio::test]
    async fn rollover_prepare_retry_preserves_matching_replacement_outcomes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old");
        old.account_id = account_id.to_string();
        store
            .save_synced_messages(account_id, "INBOX", &[old])
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account_id, "INBOX", Some(10))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();

        assert!(
            store
                .prepare_mailbox_sync(account_id, "INBOX", Some(11))
                .await
                .unwrap()
                .uid_validity_changed
        );
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 3, Some(4), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &generation,
                &[(1, false, false), (2, false, false), (3, false, false)],
            )
            .await
            .unwrap();
        let mut parsed = message("Replacement", "headers only");
        parsed.account_id = account_id.to_string();
        parsed.uid = 3;
        store
            .stage_mailbox_snapshot_message_page(account_id, "INBOX", &generation, &[parsed])
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_excluded_uids(account_id, "INBOX", &generation, &[2])
            .await
            .unwrap();
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids_without_replacement_outcome(
                    account_id,
                    "INBOX",
                    &generation,
                )
                .await
                .unwrap(),
            vec![1]
        );

        assert!(
            store
                .prepare_mailbox_sync(account_id, "INBOX", Some(11))
                .await
                .unwrap()
                .uid_validity_changed
        );
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids_without_replacement_outcome(
                    account_id,
                    "INBOX",
                    &generation,
                )
                .await
                .unwrap(),
            vec![1],
            "a retry must retain parsed and provider-excluded outcomes"
        );
    }

    #[tokio::test]
    async fn uidvalidity_replacement_catalogue_rows_cannot_retain_old_namespace_content() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old snippet");
        old.id = stable_message_id(account_id, "INBOX", 7);
        old.account_id = account_id.to_string();
        old.uid = 7;
        old.content_state = "complete".into();
        old.is_flagged = true;
        old.has_attachments = true;
        old.category = Some("travel".into());
        old.classification_confidence = Some(0.99);
        old.classification_source = Some("model".into());
        store
            .upsert_messages(std::slice::from_ref(&old))
            .await
            .unwrap();
        // Recreate fields a generic headers-only conflict intentionally keeps
        // for ordinary incremental refreshes, then prove replacement deletes
        // all of them before the new UID namespace is inserted.
        sqlx::query("UPDATE messages SET snippet = 'old snippet', body_text = 'old body', body_html = '<p>old</p>', content_state = 'complete', has_attachments = 1, category = 'travel', classification_confidence = 0.99, classification_source = 'model' WHERE id = ?")
            .bind(&old.id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .cache_message_content(&old.id, false, cached_content("old body"))
            .await
            .unwrap();

        let mut replacement = message("Replacement namespace", "new snippet");
        replacement.id = stable_message_id(account_id, "INBOX", 7);
        replacement.account_id = account_id.to_string();
        replacement.uid = 7;
        replacement.content_state = "headers_only".into();
        replacement.is_read = true;
        replacement.is_flagged = false;
        replacement.has_attachments = false;
        replacement.category = None;
        replacement.classification_confidence = None;
        replacement.classification_source = None;
        store
            .replace_uidvalidity_catalog_messages(std::slice::from_ref(&replacement))
            .await
            .unwrap();

        let stored = store
            .message_by_locator(account_id, "INBOX", 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.subject, "Replacement namespace");
        assert_eq!(stored.snippet, "new snippet");
        assert!(stored.body_text.is_empty());
        assert_eq!(stored.body_html, None);
        assert_eq!(stored.content_state, "headers_only");
        assert!(!stored.has_attachments);
        assert_eq!(stored.category, None);
        assert_eq!(stored.classification_confidence, None);
        assert_eq!(stored.classification_source, None);
        assert!(store
            .cached_message_content(&old.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn explicit_snapshot_watermark_does_not_assume_the_staged_maximum() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 10, 51, None, None),
            )
            .await
            .unwrap();
        let flags: Vec<(u32, bool, bool)> = (100..=150).map(|uid| (uid, false, false)).collect();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &flags)
            .await
            .unwrap();

        store
            .finalize_mailbox_snapshot_with_watermark(account_id, "INBOX", &generation, Some(124))
            .await
            .unwrap();
        let state = store
            .prepare_mailbox_sync(account_id, "INBOX", Some(10))
            .await
            .unwrap();
        assert!(state.initialized);
        assert_eq!(state.highest_uid, Some(124));
        assert!(!state.uid_validity_changed);
    }

    #[tokio::test]
    async fn staged_replacement_finalization_streams_metadata_and_purges_an_empty_namespace() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old");
        old.account_id = account_id.to_string();
        old.uid = 7;
        store
            .upsert_catalog_messages(std::slice::from_ref(&old))
            .await
            .unwrap();

        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 250, None, None),
            )
            .await
            .unwrap();
        let mut replacements = Vec::new();
        let mut flags = Vec::new();
        for uid in 1..=250_i64 {
            let mut replacement = message(&format!("Replacement {uid}"), "headers only");
            replacement.id = stable_message_id(account_id, "INBOX", uid as u32);
            replacement.account_id = account_id.to_string();
            replacement.uid = uid;
            replacements.push(replacement);
            flags.push((uid as u32, uid % 2 == 0, uid % 3 == 0));
        }
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &flags)
            .await
            .unwrap();
        for page in replacements.chunks(37) {
            store
                .stage_mailbox_snapshot_message_page(account_id, "INBOX", &generation, page)
                .await
                .unwrap();
        }
        store
            .finalize_mailbox_snapshot_with_staged_replacements(account_id, "INBOX", &generation)
            .await
            .unwrap();
        let uids = store.mailbox_uids(account_id, "INBOX").await.unwrap();
        assert_eq!(uids.len(), 250);
        assert!(uids.contains(&250));
        assert!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .is_some(),
            "UID 7 is the new staged replacement, not the old row"
        );

        let empty_generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 12, 1, None, None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &empty_generation,
                &[(7, false, false)],
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_excluded_uids(account_id, "INBOX", &empty_generation, &[7])
            .await
            .unwrap();
        store
            .finalize_mailbox_snapshot_with_staged_replacements(
                account_id,
                "INBOX",
                &empty_generation,
            )
            .await
            .unwrap();
        assert!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .is_none(),
            "replacement mode clears old rows even if this UID was filtered from metadata"
        );
    }

    #[tokio::test]
    async fn malformed_staged_replacement_rolls_back_the_namespace_publish() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old");
        old.account_id = account_id.to_string();
        old.uid = 7;
        store
            .upsert_catalog_messages(std::slice::from_ref(&old))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 1, None, None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(7, false, false)])
            .await
            .unwrap();
        let now = Utc::now();
        sqlx::query("INSERT INTO mailbox_snapshot_messages(account_id, mailbox, generation, uid, message_json, created_at, updated_at) VALUES (?, 'INBOX', ?, 7, '{bad json', ?, ?)")
            .bind(account_id.to_string())
            .bind(&generation)
            .bind(now)
            .bind(now)
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(store
            .finalize_mailbox_snapshot_with_staged_replacements(account_id, "INBOX", &generation)
            .await
            .is_err());
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Old namespace"
        );
        assert_eq!(
            store
                .mailbox_catalog_state(account_id, "INBOX")
                .await
                .unwrap()
                .unwrap()
                .uid_validity,
            10
        );
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            vec![7]
        );
    }

    #[tokio::test]
    async fn matching_snapshot_identity_resumes_large_partial_replacement_outcomes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "[Gmail]/All Mail",
                MailboxSnapshotIdentity::new("[Gmail]/All Mail", 7, 99_003, Some(99_004), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "[Gmail]/All Mail",
                &generation,
                &[
                    (99_003, false, false),
                    (99_002, true, false),
                    (99_001, false, false),
                ],
            )
            .await
            .unwrap();
        let mut parsed = message("Retained staged metadata", "headers only");
        parsed.id = stable_message_id(account_id, "[Gmail]/All Mail", 99_003);
        parsed.account_id = account_id.to_string();
        parsed.mailbox = "[Gmail]/All Mail".into();
        parsed.uid = 99_003;
        store
            .stage_mailbox_snapshot_message_page(
                account_id,
                "[Gmail]/All Mail",
                &generation,
                &[parsed],
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_excluded_uids(
                account_id,
                "[Gmail]/All Mail",
                &generation,
                &[99_002],
            )
            .await
            .unwrap();

        let resumed = store
            .begin_mailbox_snapshot(
                account_id,
                "[Gmail]/All Mail",
                MailboxSnapshotIdentity::new("[Gmail]/All Mail", 7, 99_003, Some(99_004), Some(42)),
            )
            .await
            .unwrap();
        assert_eq!(resumed, generation);
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "[Gmail]/All Mail",
                &resumed,
                &[
                    (99_003, false, false),
                    (99_002, true, false),
                    (99_001, false, false),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids_without_replacement_outcome(
                    account_id,
                    "[Gmail]/All Mail",
                    &resumed,
                )
                .await
                .unwrap(),
            vec![99_001],
            "already parsed and durably filtered UIDs are never fetched again"
        );
    }

    #[tokio::test]
    async fn mismatched_snapshot_identity_replaces_old_staging() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let first = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 7, 2, Some(3), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &first, &[(2, false, false)])
            .await
            .unwrap();

        let replacement = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 7, 2, Some(4), None),
            )
            .await
            .unwrap();
        assert_ne!(replacement, first);
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &first)
            .await
            .is_err());
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &replacement)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn authoritative_nonexistent_mailbox_clear_removes_the_entire_namespace() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old Sent", "old");
        old.account_id = account_id.to_string();
        old.mailbox = "Sent".into();
        old.uid = 4;
        store
            .upsert_catalog_messages(std::slice::from_ref(&old))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "Sent", "Sent", 5, 1, true)
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account_id, "Sent", Some(5))
            .await
            .unwrap();
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "Sent",
                MailboxSnapshotIdentity::new("Sent", 5, 1, Some(5), None),
            )
            .await
            .unwrap();
        store
            .record_mailbox_sync_failure(account_id, "Sent", 4, "parse", "old namespace")
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'Sent', 4, ?)")
            .bind(account_id.to_string())
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .clear_nonexistent_mailbox_namespace(account_id, "Sent")
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "Sent", 4)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .mailbox_catalog_state(account_id, "Sent")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .prepare_mailbox_sync(account_id, "Sent", Some(5))
            .await
            .unwrap()
            .highest_uid
            .is_none());
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "Sent", &generation)
            .await
            .is_err());
        assert!(store
            .mailbox_sync_failure_uids(account_id, "Sent")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn replacement_snapshot_finalization_rolls_back_replacements_when_publish_fails() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old");
        old.id = stable_message_id(account_id, "INBOX", 7);
        old.account_id = account_id.to_string();
        old.uid = 7;
        let mut collision = message("Unrelated row", "keep");
        collision.id = "replacement-id-collision".into();
        collision.account_id = account_id.to_string();
        collision.mailbox = "Archive".into();
        collision.uid = 9;
        store
            .upsert_catalog_messages(&[old.clone(), collision])
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 1, Some(8), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(7, true, false)])
            .await
            .unwrap();
        let mut replacement = message("Replacement namespace", "new");
        replacement.id = "replacement-id-collision".into();
        replacement.account_id = account_id.to_string();
        replacement.uid = 7;

        assert!(store
            .finalize_mailbox_snapshot_with_replacements(
                account_id,
                "INBOX",
                &generation,
                &[replacement],
            )
            .await
            .is_err());
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Old namespace"
        );
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            vec![7]
        );
        assert_eq!(
            store
                .mailbox_catalog_state(account_id, "INBOX")
                .await
                .unwrap()
                .unwrap()
                .uid_validity,
            10
        );
    }

    #[tokio::test]
    async fn partial_mailbox_snapshot_never_deletes_committed_messages() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut first = message("First", "one");
        first.account_id = account_id.to_string();
        first.uid = 1;
        let mut second = message("Second", "two");
        second.account_id = account_id.to_string();
        second.uid = 2;
        store
            .upsert_catalog_messages(&[first, second])
            .await
            .unwrap();

        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 77, 2, Some(3), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(2, true, false)])
            .await
            .unwrap();

        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            vec![2]
        );
        assert!(store
            .message_by_locator(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .message_by_locator(account_id, "INBOX", 2)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn finalized_mailbox_snapshot_applies_flags_deletes_absent_rows_and_clears_staging() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut stale = message("Stale", "remove me");
        stale.account_id = account_id.to_string();
        stale.uid = 1;
        let mut retained = message("Retained", "keep me");
        retained.account_id = account_id.to_string();
        retained.uid = 2;
        store
            .upsert_catalog_messages(&[stale, retained])
            .await
            .unwrap();

        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("Remote Inbox", 88, 1, Some(3), Some(u64::MAX)),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(2, true, true)])
            .await
            .unwrap();

        assert_eq!(
            store
                .finalize_mailbox_snapshot(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            1
        );
        assert!(store
            .message_by_locator(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_none());
        let retained = store
            .message_by_locator(account_id, "INBOX", 2)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.is_read);
        assert!(retained.is_flagged);
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.remote_name, "Remote Inbox");
        assert_eq!(state.uid_validity, 88);
        assert_eq!(state.remote_total, 1);
        assert!(state.historical_complete);
        assert_eq!(state.uid_next, Some(3));
        assert_eq!(
            state.highest_modseq.as_deref(),
            Some("18446744073709551615")
        );
        let sync_state = store
            .prepare_mailbox_sync(account_id, "INBOX", Some(88))
            .await
            .unwrap();
        assert!(sync_state.initialized);
        assert_eq!(sync_state.highest_uid, Some(2));
        assert!(store
            .message_by_locator(account_id, "INBOX", 2)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &generation)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn finalized_uidvalidity_rollover_clears_old_namespace_tombstones() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'INBOX', 2, ?)")
            .bind(account_id.to_string())
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store
                .prepare_mailbox_sync(account_id, "INBOX", Some(11))
                .await
                .unwrap()
                .uid_validity_changed
        );
        let pending_tombstones: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = 'INBOX'",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(pending_tombstones, 1);

        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 1, Some(3), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(2, false, false)])
            .await
            .unwrap();
        store
            .finalize_mailbox_snapshot(account_id, "INBOX", &generation)
            .await
            .unwrap();

        let tombstones: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = 'INBOX'",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(tombstones, 0);
    }

    #[tokio::test]
    async fn changed_since_delta_updates_only_observed_flags_and_persists_condstore_state() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut changed = message("Changed", "one");
        changed.account_id = account_id.to_string();
        changed.uid = 1;
        let mut omitted = message("Omitted", "two");
        omitted.account_id = account_id.to_string();
        omitted.uid = 2;
        omitted.is_read = true;
        omitted.is_flagged = true;
        store
            .upsert_catalog_messages(&[changed, omitted])
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "Remote Inbox", 77, 2, true)
            .await
            .unwrap();

        store
            .apply_complete_mailbox_changed_since_flags(
                account_id,
                "INBOX",
                MailboxChangedSinceFlags {
                    identity: MailboxSnapshotIdentity::new(
                        "Remote Inbox",
                        77,
                        2,
                        Some(3),
                        Some(u64::MAX),
                    ),
                    remote_total: 2,
                    flags: &[(1, true, true)],
                },
            )
            .await
            .unwrap();

        let changed = store
            .message_by_locator(account_id, "INBOX", 1)
            .await
            .unwrap()
            .unwrap();
        assert!(changed.is_read && changed.is_flagged);
        let omitted = store
            .message_by_locator(account_id, "INBOX", 2)
            .await
            .unwrap()
            .unwrap();
        assert!(omitted.is_read && omitted.is_flagged);
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.remote_total, 2);
        assert_eq!(state.uid_next, Some(3));
        assert_eq!(
            state.highest_modseq.as_deref(),
            Some("18446744073709551615")
        );
    }

    #[tokio::test]
    async fn changed_since_delta_supports_nomodseq_by_persisting_null_condstore_state() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 77, 0, true)
            .await
            .unwrap();

        store
            .apply_complete_mailbox_changed_since_flags(
                account_id,
                "INBOX",
                MailboxChangedSinceFlags {
                    identity: MailboxSnapshotIdentity::new("INBOX", 77, 0, None, None),
                    remote_total: 0,
                    flags: &[],
                },
            )
            .await
            .unwrap();
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.uid_next, None);
        assert_eq!(state.highest_modseq, None);
    }

    #[tokio::test]
    async fn migration_adds_nullable_condstore_columns_to_existing_catalogue_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-catalogue.db");
        let options = SqliteConnectOptions::from_str(path.to_str().unwrap())
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, created_at TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE mailbox_catalog_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, remote_total INTEGER NOT NULL DEFAULT 0, historical_complete INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))")
            .execute(&pool)
            .await
            .unwrap();
        drop(pool);

        let store = Store::open(&path).await.unwrap();
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mailbox_catalog_state)")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(columns.iter().any(|column| column.1 == "uid_next"));
        assert!(columns.iter().any(|column| column.1 == "highest_modseq"));
    }

    #[tokio::test]
    async fn mailbox_snapshot_generations_are_replaced_per_mailbox_and_isolated_between_mailboxes()
    {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let inbox_first = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 7, 2, None, None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &inbox_first, &[(2, false, false)])
            .await
            .unwrap();
        let archive = store
            .begin_mailbox_snapshot(
                account_id,
                "Archive",
                MailboxSnapshotIdentity::new("Archive", 8, 1, None, None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "Archive", &archive, &[(9, true, true)])
            .await
            .unwrap();

        let inbox_replacement = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 7, 3, None, None),
            )
            .await
            .unwrap();
        assert!(store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &inbox_first, &[(1, false, false)])
            .await
            .is_err());
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "Archive", &archive)
                .await
                .unwrap(),
            vec![9]
        );
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &inbox_replacement)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn finalized_empty_mailbox_snapshot_authoritatively_clears_a_mailbox() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut first = message("Old one", "old");
        first.account_id = account_id.to_string();
        first.uid = 1;
        let mut second = message("Old two", "old");
        second.account_id = account_id.to_string();
        second.uid = 2;
        store
            .upsert_catalog_messages(&[first, second])
            .await
            .unwrap();

        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 99, 0, Some(1), None),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .finalize_mailbox_snapshot(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            2
        );
        assert!(store
            .mailbox_uids(account_id, "INBOX")
            .await
            .unwrap()
            .is_empty());
        let state = store
            .mailbox_catalog_state(account_id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.remote_total, 0);
        assert!(state.historical_complete);
    }

    #[tokio::test]
    async fn mailbox_sync_failures_remain_retryable_until_their_uids_are_cleared() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;

        store
            .record_mailbox_sync_failure(account_id, "INBOX", 100, "parse", "bad header")
            .await
            .unwrap();
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 101, "fetch", "timed out")
            .await
            .unwrap();
        // A retry updates the diagnostic but does not create a second UID.
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 100, "fetch", "literal too large")
            .await
            .unwrap();
        assert_eq!(
            store
                .mailbox_sync_failure_uids(account_id, "INBOX")
                .await
                .unwrap(),
            vec![101, 100]
        );

        store
            .clear_mailbox_sync_failure(account_id, "INBOX", 100)
            .await
            .unwrap();
        store
            .clear_mailbox_sync_failures(account_id, "INBOX", &[101])
            .await
            .unwrap();
        assert!(store
            .mailbox_sync_failure_uids(account_id, "INBOX")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn retried_mailbox_sync_failure_yields_to_untouched_failures_in_oldest_first_order() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        for uid in 1..=50 {
            store
                .record_mailbox_sync_failure(account_id, "INBOX", uid, "fetch", "timed out")
                .await
                .unwrap();
        }
        let tie_time = Utc::now();
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 25, "fetch", "still timed out")
            .await
            .unwrap();
        sqlx::query("UPDATE mailbox_sync_failures SET updated_at = ? WHERE account_id = ? AND mailbox = 'INBOX' AND uid IN (49, 50)")
            .bind(tie_time)
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();

        let ordered = store
            .mailbox_sync_failure_uids(account_id, "INBOX")
            .await
            .unwrap();
        let retried = ordered.iter().position(|uid| *uid == 25).unwrap();
        for uid in 26..=50 {
            assert!(
                ordered
                    .iter()
                    .position(|candidate| *candidate == uid)
                    .unwrap()
                    < retried,
                "untouched UID {uid} must precede the retried UID"
            );
        }
        let forty_nine = ordered.iter().position(|uid| *uid == 49).unwrap();
        let fifty = ordered.iter().position(|uid| *uid == 50).unwrap();
        assert!(
            forty_nine < fifty,
            "equal timestamps use UID as a stable tie-breaker"
        );
    }

    #[tokio::test]
    async fn mailbox_sync_failures_are_cleared_by_index_reset_and_fenced_after_account_deletion() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 10, "parse", "bad header")
            .await
            .unwrap();
        store.reset_account_mail_index(account_id).await.unwrap();
        assert!(store
            .mailbox_sync_failure_uids(account_id, "INBOX")
            .await
            .unwrap()
            .is_empty());

        store
            .record_mailbox_sync_failure(account_id, "INBOX", 11, "fetch", "timed out")
            .await
            .unwrap();
        store.delete_account(account_id).await.unwrap();
        assert!(store
            .mailbox_sync_failure_uids(account_id, "INBOX")
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .record_mailbox_sync_failure(account_id, "INBOX", 12, "fetch", "late writer")
            .await
            .unwrap_err()
            .to_string()
            .contains("account was removed"));
    }

    #[tokio::test]
    async fn uidnext_absent_resume_rebuilds_inventory_and_keeps_current_outcomes() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 9, 2, None, None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &generation,
                &[(1, false, false), (2, false, false)],
            )
            .await
            .unwrap();
        let mut retained = message("Still present", "headers only");
        retained.account_id = account_id.to_string();
        retained.uid = 2;
        store
            .stage_mailbox_snapshot_message_page(account_id, "INBOX", &generation, &[retained])
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_excluded_uids(account_id, "INBOX", &generation, &[1])
            .await
            .unwrap();

        let resumed = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 9, 2, None, None),
            )
            .await
            .unwrap();
        assert_eq!(resumed, generation);
        assert!(store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &resumed)
            .await
            .unwrap()
            .is_empty());
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &resumed,
                &[(2, false, false), (3, false, false)],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "INBOX", &resumed)
                .await
                .unwrap(),
            vec![3, 2]
        );
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids_without_replacement_outcome(
                    account_id, "INBOX", &resumed,
                )
                .await
                .unwrap(),
            vec![3],
            "the still-present UID retains its prior staged metadata outcome"
        );
    }

    #[tokio::test]
    async fn incomplete_staged_replacement_does_not_publish_or_delete_old_namespace() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("Old namespace", "old");
        old.account_id = account_id.to_string();
        old.uid = 1;
        store
            .upsert_catalog_messages(std::slice::from_ref(&old))
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 10, 1, true)
            .await
            .unwrap();
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 11, 1, Some(2), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(1, false, false)])
            .await
            .unwrap();

        assert!(store
            .finalize_mailbox_snapshot_with_staged_replacements(account_id, "INBOX", &generation)
            .await
            .is_err());
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 1)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Old namespace"
        );
        assert_eq!(
            store
                .mailbox_catalog_state(account_id, "INBOX")
                .await
                .unwrap()
                .unwrap()
                .uid_validity,
            10
        );
        assert_eq!(
            store
                .staged_mailbox_snapshot_uids(account_id, "INBOX", &generation)
                .await
                .unwrap(),
            vec![1]
        );
    }

    #[tokio::test]
    async fn staging_replacement_outcomes_clears_their_durable_failures_atomically() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 1, 2, Some(3), None),
            )
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &generation,
                &[(1, false, false), (2, false, false)],
            )
            .await
            .unwrap();
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 1, "parse", "filtered later")
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_excluded_uids(account_id, "INBOX", &generation, &[1])
            .await
            .unwrap();
        let mut parsed = message("Parsed", "headers only");
        parsed.account_id = account_id.to_string();
        parsed.uid = 2;
        store
            .record_mailbox_sync_failure(account_id, "INBOX", 2, "parse", "fixed later")
            .await
            .unwrap();
        store
            .stage_mailbox_snapshot_message_page(account_id, "INBOX", &generation, &[parsed])
            .await
            .unwrap();
        assert!(store
            .mailbox_sync_failure_uids(account_id, "INBOX")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn complete_family_keep_set_prunes_only_obsolete_provider_keys() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        for (mailbox, uid) in [
            ("Sent", 1),
            ("Sent::Legacy", 2),
            ("Sent::A", 3),
            ("Sent::B", 4),
        ] {
            let mut message = message(mailbox, "old provider spelling");
            message.account_id = account_id.to_string();
            message.mailbox = mailbox.into();
            message.uid = uid;
            store.upsert_catalog_messages(&[message]).await.unwrap();
            store
                .save_mailbox_catalog_state(account_id, mailbox, mailbox, 5, 1, true)
                .await
                .unwrap();
        }
        for (mailbox, uid) in [("Sent::A", 3), ("Sent::B", 4)] {
            let generation = store
                .begin_mailbox_snapshot(
                    account_id,
                    mailbox,
                    MailboxSnapshotIdentity::new(mailbox, 5, 1, Some(5), None),
                )
                .await
                .unwrap();
            store
                .stage_mailbox_snapshot_page(
                    account_id,
                    mailbox,
                    &generation,
                    &[(uid, true, false)],
                )
                .await
                .unwrap();
            store
                .finalize_mailbox_snapshot(account_id, mailbox, &generation)
                .await
                .unwrap();
        }

        // One sibling's completed plan must not erase another or a legacy
        // key before the caller has a complete family keep-set.
        assert!(store
            .message_by_locator(account_id, "Sent::Legacy", 2)
            .await
            .unwrap()
            .is_some());
        store
            .prune_obsolete_mailbox_family_namespaces(
                account_id,
                "Sent",
                &["Sent::A".into(), "Sent::B".into()],
            )
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "Sent", 1)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "Sent::Legacy", 2)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "Sent::A", 3)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .message_by_locator(account_id, "Sent::B", 4)
            .await
            .unwrap()
            .is_some());
        let old_state_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mailbox_catalog_state WHERE account_id = ? AND mailbox IN ('Sent', 'Sent::Legacy')",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(old_state_count, 0);

        store
            .prune_obsolete_mailbox_family_namespaces(account_id, "Sent", &[])
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "Sent::A", 3)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account_id, "Sent::B", 4)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn nonexistent_mailbox_clear_isolated_to_the_confirmed_key() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        for (mailbox, uid) in [
            ("Archive::Missing", 1),
            ("Archive::Current", 2),
            ("Spam::Missing", 3),
            ("Spam::Current", 4),
            ("Trash::Missing", 5),
            ("Trash::Current", 6),
            ("Sent::Missing", 7),
            ("Sent::Current", 8),
            ("INBOX", 9),
            ("INBOX::Current", 10),
        ] {
            let mut message = message(mailbox, "gone");
            message.account_id = account_id.to_string();
            message.mailbox = mailbox.into();
            message.uid = uid;
            store.upsert_catalog_messages(&[message]).await.unwrap();
        }
        for (missing, current) in [
            ("Archive::Missing", "Archive::Current"),
            ("Spam::Missing", "Spam::Current"),
            ("Trash::Missing", "Trash::Current"),
            ("Sent::Missing", "Sent::Current"),
            ("INBOX", "INBOX::Current"),
        ] {
            store
                .clear_nonexistent_mailbox_namespace(account_id, missing)
                .await
                .unwrap();
            let removed: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM messages WHERE account_id = ? AND mailbox = ?",
            )
            .bind(account_id.to_string())
            .bind(missing)
            .fetch_one(&store.pool)
            .await
            .unwrap();
            assert_eq!(removed, 0);
            assert!(store
                .message_by_locator(
                    account_id,
                    current,
                    match current {
                        "Archive::Current" => 2,
                        "Spam::Current" => 4,
                        "Trash::Current" => 6,
                        "Sent::Current" => 8,
                        _ => 10,
                    },
                )
                .await
                .unwrap()
                .is_some());
        }
    }

    #[tokio::test]
    async fn account_and_reset_rebuild_job_commit_together() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "atomic-reset@example.test");
        let job = MailRebuildJob {
            account_id,
            phase: "queued".into(),
            completed: 0,
            total: None,
            reset_before_sync: true,
        };
        store
            .save_account_with_reset_mail_rebuild_job(&account, &job)
            .await
            .unwrap();
        assert!(store.account(account_id).await.unwrap().is_some());
        let saved_jobs = store.mail_rebuild_jobs().await.unwrap();
        assert_eq!(saved_jobs.len(), 1);
        assert_eq!(saved_jobs[0].account_id, job.account_id);
        assert!(saved_jobs[0].reset_before_sync);

        let uncommitted_id = uuid::Uuid::new_v4();
        let uncommitted = account_with_id(uncommitted_id, "must-not-save@example.test");
        let wrong_job = MailRebuildJob {
            account_id: uuid::Uuid::new_v4(),
            phase: "queued".into(),
            completed: 0,
            total: None,
            reset_before_sync: true,
        };
        assert!(store
            .save_account_with_reset_mail_rebuild_job(&uncommitted, &wrong_job)
            .await
            .is_err());
        assert!(store.account(uncommitted_id).await.unwrap().is_none());
        let mut changed_existing = account.clone();
        changed_existing.email = "must-not-update@example.test".into();
        assert!(store
            .save_account_with_reset_mail_rebuild_job(&changed_existing, &wrong_job)
            .await
            .is_err());
        assert_eq!(
            store.account(account_id).await.unwrap().unwrap().email,
            "atomic-reset@example.test"
        );
        assert!(store
            .save_account_with_reset_mail_rebuild_job(
                &uncommitted,
                &MailRebuildJob {
                    account_id: uncommitted_id,
                    phase: "queued".into(),
                    completed: 0,
                    total: None,
                    reset_before_sync: false,
                },
            )
            .await
            .is_err());
        assert!(store.account(uncommitted_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn account_reset_and_previous_secret_rotation_are_atomic() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let initial = account_with_id(account_id, "before-rotation@example.test");
        store.save_account(&initial).await.unwrap();
        store
            .save_mail_rebuild_job(&MailRebuildJob {
                account_id,
                phase: "old-job".into(),
                completed: 1,
                total: Some(2),
                reset_before_sync: false,
            })
            .await
            .unwrap();
        store
            .set_secret("rotation-old", "old-secret")
            .await
            .unwrap();
        store
            .set_secret("rotation-new", "new-secret")
            .await
            .unwrap();
        let mut rotated = initial.clone();
        rotated.email = "after-rotation@example.test".into();
        let reset_job = MailRebuildJob {
            account_id,
            phase: "new-job".into(),
            completed: 0,
            total: None,
            reset_before_sync: true,
        };
        store
            .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                &rotated,
                &reset_job,
                Some("rotation-old"),
                "rotation-new",
            )
            .await
            .unwrap();
        assert_eq!(
            store.account(account_id).await.unwrap().unwrap().email,
            "after-rotation@example.test"
        );
        assert_eq!(store.mail_rebuild_jobs().await.unwrap()[0].phase, "new-job");
        assert!(store.secret("rotation-old").await.unwrap().is_none());
        assert_eq!(
            store.secret("rotation-new").await.unwrap().as_deref(),
            Some("new-secret")
        );

        let rollback_id = uuid::Uuid::new_v4();
        let rollback_initial = account_with_id(rollback_id, "before-rollback@example.test");
        store.save_account(&rollback_initial).await.unwrap();
        store
            .save_mail_rebuild_job(&MailRebuildJob {
                account_id: rollback_id,
                phase: "old-job".into(),
                completed: 1,
                total: Some(2),
                reset_before_sync: false,
            })
            .await
            .unwrap();
        store
            .set_secret("rollback-old", "old-secret")
            .await
            .unwrap();
        store
            .set_secret("rollback-new", "new-secret")
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_rotation_secret_delete BEFORE DELETE ON credentials WHEN OLD.name = 'rollback-old' BEGIN SELECT RAISE(ABORT, 'forced credential cleanup failure'); END")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut rollback_rotated = rollback_initial.clone();
        rollback_rotated.email = "must-not-commit@example.test".into();
        assert!(store
            .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                &rollback_rotated,
                &MailRebuildJob {
                    account_id: rollback_id,
                    phase: "must-not-commit".into(),
                    completed: 0,
                    total: None,
                    reset_before_sync: true,
                },
                Some("rollback-old"),
                "rollback-new",
            )
            .await
            .is_err());
        assert_eq!(
            store.account(rollback_id).await.unwrap().unwrap().email,
            "before-rollback@example.test"
        );
        assert_eq!(
            store
                .mail_rebuild_jobs()
                .await
                .unwrap()
                .into_iter()
                .find(|job| job.account_id == rollback_id)
                .unwrap()
                .phase,
            "old-job"
        );
        assert_eq!(
            store.secret("rollback-old").await.unwrap().as_deref(),
            Some("old-secret")
        );
        assert_eq!(
            store.secret("rollback-new").await.unwrap().as_deref(),
            Some("new-secret")
        );

        let deleted_id = uuid::Uuid::new_v4();
        let deleted_account = account_with_id(deleted_id, "deleted@example.test");
        store.save_account(&deleted_account).await.unwrap();
        store.set_secret("deleted-old", "old-secret").await.unwrap();
        store.set_secret("deleted-new", "new-secret").await.unwrap();
        store.delete_account(deleted_id).await.unwrap();
        assert!(store
            .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                &deleted_account,
                &MailRebuildJob {
                    account_id: deleted_id,
                    phase: "must-not-revive".into(),
                    completed: 0,
                    total: None,
                    reset_before_sync: true,
                },
                Some("deleted-old"),
                "deleted-new",
            )
            .await
            .is_err());
        assert_eq!(
            store.secret("deleted-old").await.unwrap().as_deref(),
            Some("old-secret")
        );
    }
}
impl Store {
    /// Starts or resumes an incomplete mailbox inventory. Matching mailbox
    /// identity retains its staged flags and replacement outcomes across a
    /// cancelled connection; a changed identity replaces only staging.
    pub async fn begin_mailbox_snapshot(
        &self,
        account_id: AccountId,
        mailbox: &str,
        identity: MailboxSnapshotIdentity<'_>,
    ) -> Result<String> {
        let MailboxSnapshotIdentity {
            remote_name,
            uid_validity,
            initial_exists,
            uid_next,
            highest_modseq,
        } = identity;
        let account_id = account_id.to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let uid_next = uid_next.map(i64::from);
        let highest_modseq = highest_modseq.map(|value| value.to_string());
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_removed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
        let account_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
                .bind(&account_id)
                .fetch_one(&mut *tx)
                .await?;
        if account_removed || !account_exists {
            tx.rollback().await?;
            return Err(anyhow!(if account_removed {
                "account was removed"
            } else {
                "account does not exist"
            }));
        }
        let existing: Option<(String, String, i64, i64, Option<i64>)> = sqlx::query_as(
            "SELECT generation, remote_name, uid_validity, initial_exists, uid_next FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((
            existing_generation,
            existing_remote_name,
            existing_uid_validity,
            existing_exists,
            existing_uid_next,
        )) = existing
        {
            if existing_remote_name == remote_name
                && existing_uid_validity == i64::from(uid_validity)
                && existing_exists == i64::from(initial_exists)
                && existing_uid_next == uid_next
            {
                sqlx::query("UPDATE mailbox_snapshot_generations SET highest_modseq = ?, updated_at = ? WHERE account_id = ? AND mailbox = ? AND generation = ?")
                    .bind(highest_modseq)
                    .bind(Utc::now())
                    .bind(&account_id)
                    .bind(mailbox)
                    .bind(&existing_generation)
                    .execute(&mut *tx)
                    .await?;
                // A resumed full inventory must replace its old UID list.
                // Metadata and outcomes survive, then reattach only when the
                // current bounded inventory restages their UID.
                sqlx::query("DELETE FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ?")
                    .bind(&account_id)
                    .bind(mailbox)
                    .bind(&existing_generation)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(existing_generation);
            }
        }
        sqlx::query(
            "DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .execute(&mut *tx)
        .await?;
        let now = Utc::now();
        sqlx::query("INSERT INTO mailbox_snapshot_generations(account_id, mailbox, generation, remote_name, uid_validity, initial_exists, uid_next, highest_modseq, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&account_id)
            .bind(mailbox)
            .bind(&generation)
            .bind(remote_name)
            .bind(i64::from(uid_validity))
            .bind(i64::from(initial_exists))
            .bind(uid_next)
            // SQLite integers are signed, while IMAP mod-sequences are
            // unsigned 64-bit values. Text preserves every valid value.
            .bind(highest_modseq)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Writes one completed IMAP response page into a snapshot generation.
    /// Repeating a UID is safe: the most recently observed flag tuple wins.
    pub async fn stage_mailbox_snapshot_page(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        flags: &[(u32, bool, bool)],
    ) -> Result<()> {
        if flags.iter().any(|(uid, _, _)| *uid == 0) {
            return Err(anyhow!("mailbox snapshot contains UID 0"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_one(&mut *tx)
        .await?;
        if !active {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        }
        let now = Utc::now();
        for (uid, is_read, is_flagged) in flags {
            sqlx::query("INSERT INTO mailbox_snapshot_items(account_id, mailbox, generation, uid, is_read, is_flagged, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET is_read=excluded.is_read, is_flagged=excluded.is_flagged, updated_at=excluded.updated_at")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(i64::from(*uid))
                .bind(*is_read)
                .bind(*is_flagged)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE mailbox_snapshot_generations SET updated_at = ? WHERE account_id = ? AND mailbox = ? AND generation = ?")
            .bind(now)
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Durably stages one bounded page of parsed replacement metadata. JSON is
    /// intentionally stored per UID so a 100k-message replacement never needs
    /// an in-memory `Vec<MailSummary>` at finalization time.
    pub async fn stage_mailbox_snapshot_message_page(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        messages: &[MailSummary],
    ) -> Result<()> {
        let account_id = account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_id || message.mailbox != mailbox)
        {
            return Err(anyhow!(
                "snapshot replacement message does not match the requested account or mailbox"
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_one(&mut *tx)
        .await?;
        if !active {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        }
        let now = Utc::now();
        for message in messages {
            let staged: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?)",
            )
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(message.uid)
            .fetch_one(&mut *tx)
            .await?;
            if !staged {
                tx.rollback().await?;
                return Err(anyhow!(
                    "replacement message UID is absent from the snapshot"
                ));
            }
            sqlx::query("INSERT INTO mailbox_snapshot_messages(account_id, mailbox, generation, uid, message_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET message_json=excluded.message_json, updated_at=excluded.updated_at")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(message.uid)
                .bind(serde_json::to_string(message).context("serialize snapshot replacement message")?)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO mailbox_snapshot_replacement_outcomes(account_id, mailbox, generation, uid, outcome, created_at, updated_at) VALUES (?, ?, ?, ?, 'message', ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET outcome=excluded.outcome, updated_at=excluded.updated_at")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(message.uid)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(&account_id)
                .bind(mailbox)
                .bind(message.uid)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Returns every staged UID newest first. This is only an inventory view,
    /// never proof that the generation can delete absent local messages.
    pub async fn staged_mailbox_snapshot_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<Vec<u32>> {
        let account_id = account_id.to_string();
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;
        if !active {
            return Err(anyhow!("mailbox snapshot generation is not active"));
        }
        let uids: Vec<i64> = sqlx::query_scalar("SELECT uid FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ? ORDER BY uid DESC")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .fetch_all(&self.pool)
            .await?;
        uids.into_iter()
            .map(|uid| u32::try_from(uid).context("staged mailbox UID is invalid"))
            .collect()
    }

    /// Returns staged UIDs that have no local message metadata yet, newest
    /// first. Callers can fetch these headers without re-fetching rows that
    /// were already committed by a prior incremental sync.
    pub async fn staged_mailbox_snapshot_uids_needing_messages(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<Vec<u32>> {
        let account_id = account_id.to_string();
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;
        if !active {
            return Err(anyhow!("mailbox snapshot generation is not active"));
        }
        let uids: Vec<i64> = sqlx::query_scalar("SELECT staged.uid FROM mailbox_snapshot_items AS staged LEFT JOIN messages AS message ON message.account_id = staged.account_id AND message.mailbox = staged.mailbox AND message.uid = staged.uid WHERE staged.account_id = ? AND staged.mailbox = ? AND staged.generation = ? AND message.id IS NULL ORDER BY staged.uid DESC")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .fetch_all(&self.pool)
            .await?;
        uids.into_iter()
            .map(|uid| u32::try_from(uid).context("staged mailbox UID is invalid"))
            .collect()
    }

    /// Persists provider-filtered UIDs as deliberate replacement outcomes.
    /// They remain part of the authoritative flag inventory but must not be
    /// repeatedly fetched as missing metadata on each resumed connection.
    pub async fn stage_mailbox_snapshot_excluded_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        uids: &[u32],
    ) -> Result<()> {
        if uids.contains(&0) {
            return Err(anyhow!("snapshot excluded outcome contains UID 0"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for uid in uids {
            let staged: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?)",
            )
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(i64::from(*uid))
            .fetch_one(&mut *tx)
            .await?;
            if !staged {
                tx.rollback().await?;
                return Err(anyhow!(
                    "excluded replacement UID is absent from the snapshot"
                ));
            }
            let now = Utc::now();
            sqlx::query("INSERT INTO mailbox_snapshot_replacement_outcomes(account_id, mailbox, generation, uid, outcome, created_at, updated_at) VALUES (?, ?, ?, ?, 'excluded', ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET outcome=excluded.outcome, updated_at=excluded.updated_at")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(i64::from(*uid))
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM mailbox_snapshot_messages WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(i64::from(*uid))
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(&account_id)
                .bind(mailbox)
                .bind(i64::from(*uid))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Returns replacement UIDs that have not yet received either staged
    /// metadata or a durable provider-excluded outcome, newest first.
    pub async fn staged_mailbox_snapshot_uids_without_replacement_outcome(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<Vec<u32>> {
        let account_id = account_id.to_string();
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?)",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;
        if !active {
            return Err(anyhow!("mailbox snapshot generation is not active"));
        }
        let uids: Vec<i64> = sqlx::query_scalar("SELECT item.uid FROM mailbox_snapshot_items AS item LEFT JOIN mailbox_snapshot_replacement_outcomes AS outcome ON outcome.account_id = item.account_id AND outcome.mailbox = item.mailbox AND outcome.generation = item.generation AND outcome.uid = item.uid WHERE item.account_id = ? AND item.mailbox = ? AND item.generation = ? AND outcome.uid IS NULL ORDER BY item.uid DESC")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .fetch_all(&self.pool)
            .await?;
        uids.into_iter()
            .map(|uid| u32::try_from(uid).context("staged mailbox UID is invalid"))
            .collect()
    }

    /// Makes a fully observed snapshot authoritative. Flags, absence-based
    /// deletions, catalogue state, and staging cleanup commit together. The
    /// caller must invoke this only after every page has received tagged OK
    /// and mailbox identity remained stable.
    pub async fn finalize_mailbox_snapshot(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::None,
            SnapshotFinalizeWatermark::StagedMaximum,
        )
        .await
    }

    /// Atomically publishes a completed snapshot and any metadata decoded in
    /// its replacement UIDVALIDITY namespace. This is the only safe publish
    /// path when numerical UIDs can overlap the prior namespace.
    pub async fn finalize_mailbox_snapshot_with_replacements(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        replacements: &[MailSummary],
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::InMemory(replacements),
            SnapshotFinalizeWatermark::StagedMaximum,
        )
        .await
    }

    /// Finalizes a snapshot using the caller-proven contiguous realtime
    /// watermark. `None` explicitly records that this snapshot established no
    /// safe incremental watermark, even if staged UIDs are present.
    pub async fn finalize_mailbox_snapshot_with_replacements_and_watermark(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        replacements: &[MailSummary],
        watermark: Option<u32>,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::InMemory(replacements),
            SnapshotFinalizeWatermark::Explicit(watermark),
        )
        .await
    }

    /// Atomically publishes a replacement namespace from durable, paged
    /// generation staging rather than retaining all decoded messages in RAM.
    pub async fn finalize_mailbox_snapshot_with_staged_replacements(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::Staged,
            SnapshotFinalizeWatermark::StagedMaximum,
        )
        .await
    }

    /// Realtime replacement finalization with a caller-proven contiguous
    /// watermark. `None` is preserved as an explicit no-watermark outcome.
    pub async fn finalize_mailbox_snapshot_with_staged_replacements_and_watermark(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        watermark: Option<u32>,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::Staged,
            SnapshotFinalizeWatermark::Explicit(watermark),
        )
        .await
    }

    /// Realtime callers without replacement metadata can use an explicit
    /// contiguous watermark without constructing an empty replacement slice.
    pub async fn finalize_mailbox_snapshot_with_watermark(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        watermark: Option<u32>,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::None,
            SnapshotFinalizeWatermark::Explicit(watermark),
        )
        .await
    }

    async fn finalize_mailbox_snapshot_inner(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
        replacement_publication: SnapshotReplacementPublication<'_>,
        watermark: SnapshotFinalizeWatermark,
    ) -> Result<u64> {
        let account_id = account_id.to_string();
        if let SnapshotReplacementPublication::InMemory(replacements) = replacement_publication {
            if replacements
                .iter()
                .any(|message| message.account_id != account_id || message.mailbox != mailbox)
            {
                return Err(anyhow!(
                    "replacement message does not match the finalized account or mailbox"
                ));
            }
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_removed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
        if account_removed {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        let generation_state: Option<MailboxSnapshotGenerationState> = sqlx::query_as(
            "SELECT remote_name, uid_validity, initial_exists, uid_next, highest_modseq FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((remote_name, uid_validity, initial_exists, uid_next, highest_modseq)) =
            generation_state
        else {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        };
        let prior_uid_validities: Vec<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? UNION SELECT uid_validity FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ? AND uid_validity IS NOT NULL",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(&account_id)
        .bind(mailbox)
        .fetch_all(&mut *tx)
        .await?;
        let replacement_namespace = !matches!(
            replacement_publication,
            SnapshotReplacementPublication::None
        );
        if let SnapshotReplacementPublication::InMemory(replacements) = replacement_publication {
            for replacement in replacements {
                let staged: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?)",
                )
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(replacement.uid)
                .fetch_one(&mut *tx)
                .await?;
                if !staged {
                    tx.rollback().await?;
                    return Err(anyhow!(
                        "replacement message UID is absent from the finalized snapshot"
                    ));
                }
            }
        }
        if matches!(
            replacement_publication,
            SnapshotReplacementPublication::Staged
        ) {
            let missing_outcome: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items AS item LEFT JOIN mailbox_snapshot_replacement_outcomes AS outcome ON outcome.account_id = item.account_id AND outcome.mailbox = item.mailbox AND outcome.generation = item.generation AND outcome.uid = item.uid WHERE item.account_id = ? AND item.mailbox = ? AND item.generation = ? AND outcome.uid IS NULL)",
            )
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .fetch_one(&mut *tx)
            .await?;
            let message_outcome_without_json: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_items AS item JOIN mailbox_snapshot_replacement_outcomes AS outcome ON outcome.account_id = item.account_id AND outcome.mailbox = item.mailbox AND outcome.generation = item.generation AND outcome.uid = item.uid AND outcome.outcome = 'message' LEFT JOIN mailbox_snapshot_messages AS message ON message.account_id = item.account_id AND message.mailbox = item.mailbox AND message.generation = item.generation AND message.uid = item.uid WHERE item.account_id = ? AND item.mailbox = ? AND item.generation = ? AND message.uid IS NULL)",
            )
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .fetch_one(&mut *tx)
            .await?;
            if missing_outcome || message_outcome_without_json {
                tx.rollback().await?;
                return Err(anyhow!(
                    "staged replacement snapshot is incomplete and cannot be published"
                ));
            }
        }
        if replacement_namespace {
            clear_uidvalidity_replacement_namespace_in_transaction(&mut tx, &account_id, mailbox)
                .await?;
        }
        match replacement_publication {
            SnapshotReplacementPublication::None => {}
            SnapshotReplacementPublication::InMemory(replacements) => {
                for replacement in replacements {
                    persist_message(&mut tx, replacement).await?;
                }
            }
            SnapshotReplacementPublication::Staged => {
                persist_staged_uidvalidity_replacement_messages_in_transaction(
                    &mut tx,
                    &account_id,
                    mailbox,
                    generation,
                )
                .await?;
            }
        }
        sqlx::query("DELETE FROM message_content_cache WHERE message_id IN (SELECT message.id FROM messages AS message JOIN mailbox_snapshot_items AS staged ON staged.account_id = message.account_id AND staged.mailbox = message.mailbox AND staged.uid = message.uid WHERE staged.account_id = ? AND staged.mailbox = ? AND staged.generation = ? AND staged.is_flagged = 1)")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        for table in ["starred_message_bodies", "starred_attachment_metadata"] {
            let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT message.id FROM messages AS message JOIN mailbox_snapshot_items AS staged ON staged.account_id = message.account_id AND staged.mailbox = message.mailbox AND staged.uid = message.uid WHERE staged.account_id = ? AND staged.mailbox = ? AND staged.generation = ? AND staged.is_flagged = 0)");
            sqlx::query(&statement)
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE messages SET is_read = (SELECT staged.is_read FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?), is_flagged = (SELECT staged.is_flagged FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?) WHERE account_id = ? AND mailbox = ? AND EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?)")
            .bind(generation)
            .bind(generation)
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.generation = ? AND staged.uid = messages.uid)")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // A complete authoritative snapshot also proves that failures for
        // UIDs no longer present remotely are obsolete. Failures for staged
        // UIDs remain until their metadata fetch succeeds explicitly.
        sqlx::query("DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = mailbox_sync_failures.account_id AND staged.mailbox = mailbox_sync_failures.mailbox AND staged.generation = ? AND staged.uid = mailbox_sync_failures.uid)")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, remote_total=excluded.remote_total, historical_complete=excluded.historical_complete, uid_next=excluded.uid_next, highest_modseq=excluded.highest_modseq, updated_at=excluded.updated_at")
            .bind(&account_id)
            .bind(mailbox)
            .bind(remote_name)
            .bind(uid_validity)
            .bind(initial_exists)
            .bind(uid_next)
            .bind(highest_modseq)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        if prior_uid_validities
            .iter()
            .any(|previous| *previous != uid_validity)
        {
            sqlx::query(
                "DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?",
            )
            .bind(&account_id)
            .bind(mailbox)
            .execute(&mut *tx)
            .await?;
        }
        let highest_uid = match watermark {
            SnapshotFinalizeWatermark::StagedMaximum => {
                sqlx::query_scalar(
                    "SELECT MAX(uid) FROM mailbox_snapshot_items WHERE account_id = ? AND mailbox = ? AND generation = ?",
                )
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .fetch_one(&mut *tx)
                .await?
            }
            SnapshotFinalizeWatermark::Explicit(watermark) => watermark.map(i64::from),
        };
        sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid, uid_validity) VALUES (?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET initialized_at=excluded.initialized_at, highest_uid=excluded.highest_uid, uid_validity=excluded.uid_validity")
            .bind(&account_id)
            .bind(mailbox)
            .bind(Utc::now())
            .bind(highest_uid)
            .bind(uid_validity)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?")
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if deleted > 0 || replacement_namespace {
            self.rebuild_threads_for_account(&account_id).await?;
        }
        Ok(deleted)
    }

    /// Discards an incomplete or invalidated snapshot without touching the
    /// committed mailbox. This is the cancellation and identity-change path.
    pub async fn discard_mailbox_snapshot(
        &self,
        account_id: AccountId,
        mailbox: &str,
        generation: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(generation)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records a retryable per-UID failure. A later failure for the same UID
    /// replaces its diagnostic while preserving the fact that it still needs
    /// a fetch, even if newer UIDs were successfully committed.
    pub async fn record_mailbox_sync_failure(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
        stage: &str,
        error: &str,
    ) -> Result<()> {
        if uid == 0 {
            return Err(anyhow!("mailbox sync failure contains UID 0"));
        }
        sqlx::query("INSERT INTO mailbox_sync_failures(account_id, mailbox, uid, stage, error, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET stage=excluded.stage, error=excluded.error, updated_at=excluded.updated_at")
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(i64::from(uid))
            .bind(stage)
            .bind(error)
            .bind(Utc::now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Returns outstanding failures oldest first. Retrying a permanent
    /// failure refreshes its timestamp, allowing untouched failures to make
    /// progress on later reconnects instead of starving behind one UID.
    pub async fn mailbox_sync_failure_uids(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Vec<u32>> {
        let uids: Vec<i64> = sqlx::query_scalar(
            "SELECT uid FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? ORDER BY updated_at ASC, uid ASC",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_all(&self.pool)
        .await?;
        uids.into_iter()
            .map(|uid| u32::try_from(uid).context("stored failed mailbox UID is invalid"))
            .collect()
    }

    /// Marks one previously failed UID as successfully published.
    pub async fn clear_mailbox_sync_failure(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
    ) -> Result<()> {
        sqlx::query(
            "DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND uid = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(i64::from(uid))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clears a page of successfully published UIDs. An empty page is a
    /// no-op, which makes callers safe to invoke this after every batch.
    pub async fn clear_mailbox_sync_failures(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uids: &[u32],
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; uids.len()].join(",");
        let statement = format!(
            "DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND uid IN ({placeholders})"
        );
        let mut query = sqlx::query(&statement)
            .bind(account_id.to_string())
            .bind(mailbox);
        for uid in uids {
            query = query.bind(i64::from(*uid));
        }
        query.execute(&self.pool).await?;
        Ok(())
    }

    /// Commits a complete CONDSTORE `CHANGEDSINCE` flag result. Unlike a full
    /// snapshot, omitted UIDs are not evidence of deletion and are left
    /// untouched. Call this only after the IMAP command received tagged OK
    /// and the caller has confirmed the selected mailbox identity is stable.
    pub async fn apply_complete_mailbox_changed_since_flags(
        &self,
        account_id: AccountId,
        mailbox: &str,
        changed_since: MailboxChangedSinceFlags<'_>,
    ) -> Result<()> {
        let MailboxChangedSinceFlags {
            identity:
                MailboxSnapshotIdentity {
                    remote_name,
                    uid_validity,
                    uid_next,
                    highest_modseq,
                    ..
                },
            remote_total,
            flags,
        } = changed_since;
        if flags.iter().any(|(uid, _, _)| *uid == 0) {
            return Err(anyhow!("CHANGEDSINCE flag delta contains UID 0"));
        }
        let remote_total =
            i64::try_from(remote_total).context("remote mailbox total is invalid")?;
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: Option<(String, i64, bool)> = sqlx::query_as(
            "SELECT remote_name, uid_validity, historical_complete FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_optional(&mut *tx)
        .await?;
        if current.as_ref() != Some(&(remote_name.to_owned(), i64::from(uid_validity), true)) {
            tx.rollback().await?;
            return Err(anyhow!(
                "cannot apply CHANGEDSINCE delta without a stable mailbox catalogue"
            ));
        }
        for (uid, is_read, is_flagged) in flags {
            sqlx::query("UPDATE messages SET is_read = ?, is_flagged = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(is_read)
                .bind(is_flagged)
                .bind(&account_id)
                .bind(mailbox)
                .bind(i64::from(*uid))
                .execute(&mut *tx)
                .await?;
            if *is_flagged {
                sqlx::query("DELETE FROM message_content_cache WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)")
                    .bind(&account_id)
                    .bind(mailbox)
                    .bind(i64::from(*uid))
                    .execute(&mut *tx)
                    .await?;
            } else {
                for table in ["starred_message_bodies", "starred_attachment_metadata"] {
                    let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?)");
                    sqlx::query(&statement)
                        .bind(&account_id)
                        .bind(mailbox)
                        .bind(i64::from(*uid))
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        let updated = sqlx::query("UPDATE mailbox_catalog_state SET remote_total = ?, uid_next = ?, highest_modseq = ?, updated_at = ? WHERE account_id = ? AND mailbox = ? AND remote_name = ? AND uid_validity = ?")
            .bind(remote_total)
            .bind(uid_next.map(i64::from))
            .bind(highest_modseq.map(|value| value.to_string()))
            .bind(Utc::now())
            .bind(&account_id)
            .bind(mailbox)
            .bind(remote_name)
            .bind(i64::from(uid_validity))
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!(
                "mailbox identity changed while applying CHANGEDSINCE delta"
            ));
        }
        tx.commit().await?;
        Ok(())
    }
}
