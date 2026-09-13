use crate::{
    account::Account, mail_metrics::PublicationTransactionTimer, provider, AccountAuth, AccountId,
};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Utc};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM},
    rand::{SecureRandom, SystemRandom},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    FromRow, SqlitePool,
};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, FileTimes, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime},
};

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
/// Account deletion waits only for a bounded SMTP safe point. The shared
/// database gate expires if its owner crashes, so a live account is never
/// permanently stranded behind an abandoned removal attempt.
const ACCOUNT_REMOVAL_GATE_LEASE_SECONDS: i64 = 120;
const OPERATION_CLAIM_LEASE_SECONDS: i64 = 90;
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

/// Keeps one submitted operation lease live while its owner is inside a
/// provider call. Dropping the guard aborts the heartbeat; terminal journal
/// transitions still remain the caller's explicit responsibility.
pub struct OperationClaimHeartbeat {
    task: tokio::task::JoinHandle<()>,
}

pub struct OAuthRefreshLease {
    store: Store,
    secret_name: String,
    owner: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for OAuthRefreshLease {
    fn drop(&mut self) {
        self.task.abort();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (store, name, owner) = (
                self.store.clone(),
                self.secret_name.clone(),
                self.owner.clone(),
            );
            runtime.spawn(async move {
                let _ = sqlx::query(
                    "DELETE FROM oauth_refresh_leases WHERE secret_name = ? AND owner = ?",
                )
                .bind(name)
                .bind(owner)
                .execute(&store.pool)
                .await;
            });
        }
    }
}

/// Owner-scoped durable account-removal gate. The lease is renewed while a
/// caller drains in-flight work; expiry is only a crash fallback.
pub struct AccountOperationGate {
    store: Store,
    account_id: AccountId,
    owner: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AccountOperationGate {
    fn drop(&mut self) {
        self.task.abort();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let store = self.store.clone();
            let account_id = self.account_id;
            let owner = self.owner.clone();
            runtime.spawn(async move {
                let _ = store.release_account_removal_gate(account_id, &owner).await;
            });
        }
    }
}

impl AccountOperationGate {
    pub async fn release(self) -> Result<()> {
        self.task.abort();
        self.store
            .release_account_removal_gate(self.account_id, &self.owner)
            .await
            .map(|_| ())
    }

    /// Publishes configuration, credential rotation and replacement intent as
    /// one owner-fenced commit. Readers can never see old endpoints paired
    /// with the new credential, including if a process dies during the update.
    pub async fn save_account_with_secret_and_rebuild(
        &self,
        account: &Account,
        secret_name: &str,
        secret: Option<&str>,
        rebuild: Option<&MailRebuildJob>,
        previous_secret_name: Option<&str>,
    ) -> Result<()> {
        if account.id != self.account_id || rebuild.is_some_and(|job| job.account_id != account.id)
        {
            return Err(anyhow!("Account update does not match its ownership claim"));
        }
        let mut tx = self.store.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_removal_gates WHERE account_id = ? AND owner = ? AND expires_at > ?)")
            .bind(self.account_id.to_string()).bind(&self.owner).bind(Utc::now()).fetch_one(&mut *tx).await?;
        if !owned {
            tx.rollback().await?;
            return Err(anyhow!("Account update ownership expired; try again"));
        }
        save_account_in_transaction(&mut tx, account).await?;
        if let Some(secret) = secret {
            let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
            let ciphertext = encrypt_secret(&self.store.vault_key, nonce, secret_name, secret)?;
            sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(name) DO UPDATE SET nonce=excluded.nonce, ciphertext=excluded.ciphertext, updated_at=excluded.updated_at")
                .bind(secret_name).bind(nonce.as_slice()).bind(ciphertext).bind(Utc::now()).execute(&mut *tx).await?;
        }
        if let Some(job) = rebuild {
            save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        }
        if let Some(previous) = previous_secret_name.filter(|name| *name != secret_name) {
            sqlx::query("DELETE FROM credentials WHERE name = ?")
                .bind(previous)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_account_and_secret(self, secret_name: &str) -> Result<()> {
        self.store
            .delete_account_and_secret_with_removal_gate(self.account_id, secret_name, &self.owner)
            .await
    }
}

impl Drop for OperationClaimHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
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

/// Per-UID mutation versions captured before a CONDSTORE command. The delta
/// publication compares these inside its write transaction.
#[derive(Debug, Clone)]
pub struct ChangedSinceWriteReceipt {
    pub account_id: AccountId,
    pub account_config_generation: i64,
    pub account_config_fingerprint: String,
    pub mailbox: String,
    pub remote_name: String,
    pub uid_validity: u32,
    pub local_mutation_versions: Vec<ProviderMessageVersion>,
}

type MailboxSnapshotGenerationState = (
    String,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

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

/// Immutable remote locator captured immediately before an on-demand IMAP
/// fetch. Every content writer must present this receipt so a UID reused by a
/// later UIDVALIDITY namespace cannot receive bytes from the old message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, FromRow)]
pub struct MessageRemoteIdentity {
    pub message_id: String,
    pub account_id: String,
    pub account_config_generation: i64,
    pub account_config_fingerprint: String,
    pub mailbox: String,
    pub remote_name: String,
    pub uid: i64,
    pub uid_validity: i64,
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

/// Namespace and local flag versions captured immediately before a provider
/// fetch. A later commit compares both values in the same write transaction.
#[derive(Debug, Clone)]
pub struct ProviderWriteReceipt {
    pub account_id: AccountId,
    /// The exact persisted account transport configuration used to open the
    /// provider connection. A later endpoint/auth change invalidates this
    /// receipt even if another server happens to use the same UIDVALIDITY.
    pub account_config_generation: i64,
    pub account_config_fingerprint: String,
    pub mailbox: String,
    pub remote_name: String,
    pub uid_validity: u32,
    pub expected_flags: Vec<ExpectedMessageFlags>,
    pub local_mutation_versions: Vec<ProviderMessageVersion>,
}

/// Account transport configuration captured before provider I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProviderReceipt {
    pub account_id: AccountId,
    pub config_generation: i64,
    pub config_fingerprint: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ProviderMessageVersion {
    pub uid: i64,
    pub version: i64,
}

/// Durable bounded-range state for periodic Gmail label reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailLabelReconciliationCursor {
    pub cursor_uid: Option<u32>,
    pub span: u32,
}

#[derive(Debug, Clone)]
pub struct GmailInboxMembershipReceipt {
    pub account_id: AccountId,
    pub gmail_message_id: String,
    observed_at: Option<DateTime<Utc>>,
    memberships: Vec<GmailInboxMembershipVersion>,
}

/// Account-wide Inbox membership epoch captured before a Gmail header fetch.
/// It fences first-time X-GM-MSGID observations, when no per-ID receipt exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailInboxMembershipEpochReceipt {
    pub account_id: AccountId,
    pub epoch: i64,
}

/// A Gmail identity and label observation for an already-published local row.
#[derive(Debug, Clone)]
pub struct GmailMessageObservation {
    pub local_message_id: String,
    pub gmail_message_id: String,
    pub labels: Vec<String>,
}

/// Gmail labels associated with one UID in a provider header batch. Storage
/// resolves the canonical local ID only after the batch is published.
#[derive(Debug, Clone)]
pub struct GmailProviderObservation {
    pub uid: u32,
    pub gmail_message_id: String,
    pub labels: Vec<String>,
}

/// Immutable destination locator returned by COPYUID. A UID is never enough
/// to address the destination because a later UIDVALIDITY namespace can reuse
/// the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveDestinationLocator {
    pub uid_validity: u32,
    pub uid: u32,
}

impl From<u32> for MoveDestinationLocator {
    fn from(uid: u32) -> Self {
        Self {
            uid_validity: 0,
            uid,
        }
    }
}

impl From<crate::mail::MoveDestination> for MoveDestinationLocator {
    fn from(destination: crate::mail::MoveDestination) -> Self {
        Self {
            uid_validity: destination.uid_validity,
            uid: destination.uid,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct GmailInboxMembershipVersion {
    message_id: String,
    uid: i64,
    uid_validity: i64,
    version: i64,
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
    pub next_cursor: Option<MailCursor>,
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
    /// Present only for generations started from an exact provider Account.
    /// A changed endpoint or principal invalidates all later publications.
    pub account_config_generation: Option<i64>,
    pub account_config_fingerprint: Option<String>,
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
    pub error_class: String,
    pub attempt_count: i64,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub user_action_required: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct MailboxFailureSummary {
    pub outstanding: u32,
    pub retryable: u32,
    pub user_action_required: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
}

/// Durable progress for one scheduled folder. Discovery and header coverage
/// are intentionally separate: a complete UID inventory does not mean every
/// header was fetched successfully.
#[derive(Debug, Clone, FromRow)]
pub struct FolderSyncState {
    pub account_id: String,
    pub mailbox: String,
    pub remote_name: String,
    pub uid_validity: i64,
    /// Opaque generation fence. A worker must return this value with every
    /// page so an old UID namespace can never write into its replacement.
    pub generation: String,
    /// Inclusive UID boundary captured at discovery start. Rows above it are
    /// realtime work and are never candidates for absence reconciliation.
    pub upper_boundary: Option<i64>,
    /// Descending historical cursor. It is advanced atomically with each
    /// successfully committed header page.
    pub cursor: Option<i64>,
    pub discovery_complete: bool,
    pub headers_complete: bool,
    pub local_mutation_version: i64,
    pub revision: i64,
    pub retry_after: Option<DateTime<Utc>>,
    /// Present only for generations started from an exact provider Account.
    /// A changed endpoint or principal invalidates all later publications.
    pub account_config_generation: Option<i64>,
    pub account_config_fingerprint: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// An account-wide backend-owned sync run. It records independently usable
/// milestones so a background failure cannot make an authenticated account or
/// already committed headers look unavailable.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SyncRun {
    pub run_id: String,
    pub account_id: AccountId,
    pub stage: String,
    pub inbox_ready: bool,
    pub primary_complete: bool,
    pub secondary_complete: bool,
    pub deferred_complete: bool,
    pub content_loading: bool,
    pub retry_count: u32,
    pub outcome: String,
    pub revision: u64,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncRunUpdate<'a> {
    pub stage: Option<&'a str>,
    pub inbox_ready: Option<bool>,
    pub primary_complete: Option<bool>,
    pub secondary_complete: Option<bool>,
    pub deferred_complete: Option<bool>,
    pub content_loading: Option<bool>,
    pub retry_count: Option<u32>,
    pub outcome: Option<&'a str>,
    pub next_retry_at: Option<Option<DateTime<Utc>>>,
    pub error: Option<Option<&'a str>>,
}

/// Immutable remote locator captured for a user-facing mutation. A missing
/// UIDVALIDITY is allowed only for local-only operations such as SMTP staging.
#[derive(Debug, Clone)]
pub struct OperationTarget<'a> {
    pub mailbox: Option<&'a str>,
    pub uid: Option<u32>,
    pub uid_validity: Option<u64>,
    pub message_id: Option<&'a str>,
}

#[derive(Debug, Clone, FromRow)]
pub struct OperationJournalEntry {
    pub operation_id: String,
    pub account_id: String,
    pub mailbox: Option<String>,
    pub uid: Option<i64>,
    pub uid_validity: Option<i64>,
    pub message_id: Option<String>,
    pub kind: String,
    pub payload_json: String,
    pub local_version: i64,
    pub dependency_id: Option<String>,
    pub state: String,
    pub outcome: Option<String>,
    pub attempts: i64,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    #[sqlx(default)]
    pub smtp_accepted_at: Option<DateTime<Utc>>,
    /// Present only while another process owns the provider attempt.  Exposed
    /// so account lifecycle work can wait for every active remote mutation,
    /// not only SMTP submission.
    #[sqlx(default)]
    pub claim_owner: Option<String>,
    #[sqlx(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

type SyncRunRow = (
    String,
    String,
    String,
    bool,
    bool,
    bool,
    bool,
    bool,
    i64,
    String,
    i64,
    Option<DateTime<Utc>>,
    Option<String>,
);

fn sync_run_from_row(row: SyncRunRow) -> Result<SyncRun> {
    let (
        run_id,
        account_id,
        stage,
        inbox_ready,
        primary_complete,
        secondary_complete,
        deferred_complete,
        content_loading,
        retry_count,
        outcome,
        revision,
        next_retry_at,
        error,
    ) = row;
    Ok(SyncRun {
        run_id,
        account_id: AccountId::parse_str(&account_id)?,
        stage,
        inbox_ready,
        primary_complete,
        secondary_complete,
        deferred_complete,
        content_loading,
        retry_count: u32::try_from(retry_count).context("sync run retry count is invalid")?,
        outcome,
        revision: u64::try_from(revision).context("sync run revision is invalid")?,
        next_retry_at,
        error,
    })
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
            "CREATE TABLE IF NOT EXISTS accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, config_generation INTEGER NOT NULL DEFAULT 1, config_fingerprint TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS credentials (name TEXT PRIMARY KEY, nonce BLOB NOT NULL, ciphertext BLOB NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS oauth_refresh_leases (secret_name TEXT PRIMARY KEY REFERENCES credentials(name) ON DELETE CASCADE, owner TEXT NOT NULL, expires_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, message_id TEXT, in_reply_to TEXT, reference_ids TEXT, thread_id TEXT NOT NULL, threading_scanned INTEGER NOT NULL DEFAULT 1, recipient_headers_scanned INTEGER NOT NULL DEFAULT 1, subject TEXT NOT NULL, from_name TEXT, from_address TEXT NOT NULL, to_addresses TEXT NOT NULL, cc_addresses TEXT NOT NULL DEFAULT '', bcc_addresses TEXT NOT NULL DEFAULT '', reply_to_addresses TEXT NOT NULL DEFAULT '', received_at TEXT NOT NULL, snippet TEXT NOT NULL, body_text TEXT NOT NULL, unsubscribe_kind TEXT, unsubscribe_url TEXT, unsubscribe_scanned INTEGER NOT NULL DEFAULT 0, is_read INTEGER NOT NULL DEFAULT 0, is_flagged INTEGER NOT NULL DEFAULT 0, has_attachments INTEGER NOT NULL DEFAULT 0, category TEXT, classification_confidence REAL, classification_source TEXT, classification_signals TEXT NOT NULL DEFAULT '', UNIQUE(account_id, mailbox, uid))",
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, from_name, from_address, to_addresses, body_text, content='messages', content_rowid='rowid')",
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE INDEX IF NOT EXISTS messages_account_mailbox_date ON messages(account_id, mailbox, received_at DESC)",
            "CREATE TABLE IF NOT EXISTS mailbox_sync_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, initialized_at TEXT NOT NULL, highest_uid INTEGER, uid_validity INTEGER, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS mailbox_action_tombstones (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0, data BLOB NOT NULL)",
            "CREATE INDEX IF NOT EXISTS attachments_message_id ON attachments(message_id)",
            "CREATE TABLE IF NOT EXISTS starred_message_bodies (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, body_text TEXT NOT NULL, body_html TEXT, attachment_presentation_version INTEGER NOT NULL DEFAULT 0, cached_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS starred_attachment_metadata (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS message_content_cache (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, content_state TEXT NOT NULL CHECK(content_state = 'complete'), body_text TEXT NOT NULL, body_html TEXT, unsubscribe_kind TEXT, attachments_json TEXT NOT NULL, byte_size INTEGER NOT NULL CHECK(byte_size >= 0), last_accessed INTEGER NOT NULL)",
            "CREATE INDEX IF NOT EXISTS message_content_cache_lru ON message_content_cache(last_accessed, message_id)",
            "CREATE TABLE IF NOT EXISTS message_content_fetches (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, claimed_at TEXT NOT NULL, claim_owner TEXT NOT NULL DEFAULT '')",
            "CREATE TABLE IF NOT EXISTS app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS mailbox_catalog_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, remote_total INTEGER NOT NULL DEFAULT 0, historical_complete INTEGER NOT NULL DEFAULT 0, uid_next INTEGER, highest_modseq TEXT, provider_config_generation INTEGER, provider_config_fingerprint TEXT, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_generations (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, generation TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, initial_exists INTEGER NOT NULL, uid_next INTEGER, highest_modseq TEXT, account_config_generation INTEGER, account_config_fingerprint TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation))",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_items (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, is_read INTEGER NOT NULL, is_flagged INTEGER NOT NULL, local_mutation_version INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_messages (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, message_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS mailbox_snapshot_replacement_outcomes (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, outcome TEXT NOT NULL CHECK(outcome IN ('message', 'excluded')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
            "CREATE INDEX IF NOT EXISTS mailbox_snapshot_items_generation_uid ON mailbox_snapshot_items(account_id, mailbox, generation, uid DESC)",
            "CREATE TABLE IF NOT EXISTS mailbox_sync_failures (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, stage TEXT NOT NULL, error TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS mail_rebuild_jobs (account_id TEXT PRIMARY KEY, phase TEXT NOT NULL, completed INTEGER NOT NULL DEFAULT 0, total INTEGER, reset_before_sync INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS sync_runs (run_id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, stage TEXT NOT NULL, inbox_ready INTEGER NOT NULL DEFAULT 0, primary_complete INTEGER NOT NULL DEFAULT 0, secondary_complete INTEGER NOT NULL DEFAULT 0, deferred_complete INTEGER NOT NULL DEFAULT 0, content_loading INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0, outcome TEXT NOT NULL DEFAULT 'running', revision INTEGER NOT NULL DEFAULT 0, next_retry_at TEXT, error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE UNIQUE INDEX IF NOT EXISTS sync_runs_one_active_per_account ON sync_runs(account_id) WHERE outcome = 'running'",
            "CREATE TABLE IF NOT EXISTS folder_sync_state (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, generation TEXT NOT NULL, upper_boundary INTEGER, cursor INTEGER, discovery_complete INTEGER NOT NULL DEFAULT 0, headers_complete INTEGER NOT NULL DEFAULT 0, local_mutation_version INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0, retry_after TEXT, account_config_generation INTEGER, account_config_fingerprint TEXT, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS folder_sync_discovery (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox) REFERENCES folder_sync_state(account_id, mailbox) ON DELETE CASCADE)",
            "CREATE INDEX IF NOT EXISTS folder_sync_discovery_pending ON folder_sync_discovery(account_id, mailbox, generation, uid DESC)",
            "CREATE TABLE IF NOT EXISTS folder_sync_header_outcomes (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, completed_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation, uid) REFERENCES folder_sync_discovery(account_id, mailbox, generation, uid) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS mailbox_mutation_fences (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, uid_validity INTEGER, version INTEGER NOT NULL, operation_id TEXT, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS mailbox_mutation_versions (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, uid_validity INTEGER NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid, uid_validity))",
            "CREATE TABLE IF NOT EXISTS operation_journal (operation_id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT, uid INTEGER, uid_validity INTEGER, message_id TEXT, kind TEXT NOT NULL, payload_json TEXT NOT NULL, local_version INTEGER NOT NULL, dependency_id TEXT REFERENCES operation_journal(operation_id), state TEXT NOT NULL, outcome TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_retry_at TEXT, error TEXT, smtp_accepted_at TEXT, claim_owner TEXT, claimed_at TEXT, claimed_from_state TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS operation_message_backups (operation_id TEXT PRIMARY KEY REFERENCES operation_journal(operation_id) ON DELETE CASCADE, message_id TEXT NOT NULL, original_mailbox TEXT NOT NULL, original_uid INTEGER NOT NULL, message_json TEXT NOT NULL, created_at TEXT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS operation_journal_ready ON operation_journal(account_id, state, next_retry_at, created_at)",
            "CREATE TABLE IF NOT EXISTS gmail_logical_messages (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, gmail_message_id TEXT NOT NULL, labels_json TEXT NOT NULL, observed_at TEXT NOT NULL, PRIMARY KEY(account_id, gmail_message_id))",
            "CREATE TABLE IF NOT EXISTS gmail_message_memberships (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, gmail_message_id TEXT NOT NULL, PRIMARY KEY(account_id, message_id), FOREIGN KEY(account_id, gmail_message_id) REFERENCES gmail_logical_messages(account_id, gmail_message_id) ON DELETE CASCADE)",
            "CREATE TABLE IF NOT EXISTS gmail_label_reconciliation_state (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, cursor_uid INTEGER, span INTEGER NOT NULL DEFAULT 64, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS gmail_inbox_membership_epochs (account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE, epoch INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS message_temporal_observations (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, internal_date TEXT NOT NULL, message_date TEXT, observed_at TEXT NOT NULL, PRIMARY KEY(account_id, message_id))",
            "CREATE TABLE IF NOT EXISTS deleted_account_tombstones (account_id TEXT PRIMARY KEY, deleted_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS account_removal_gates (account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE, owner TEXT NOT NULL, blocked_at TEXT NOT NULL, expires_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS sent_correspondents (account_id TEXT NOT NULL, address TEXT NOT NULL COLLATE NOCASE, PRIMARY KEY(account_id, address))",
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
        let mut added_catalog_provider_stamp = false;
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
        if !catalog_columns
            .iter()
            .any(|column| column.1 == "provider_config_generation")
        {
            sqlx::query(
                "ALTER TABLE mailbox_catalog_state ADD COLUMN provider_config_generation INTEGER",
            )
            .execute(&self.pool)
            .await?;
            added_catalog_provider_stamp = true;
        }
        if !catalog_columns
            .iter()
            .any(|column| column.1 == "provider_config_fingerprint")
        {
            sqlx::query(
                "ALTER TABLE mailbox_catalog_state ADD COLUMN provider_config_fingerprint TEXT",
            )
            .execute(&self.pool)
            .await?;
            added_catalog_provider_stamp = true;
        }
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
            "CREATE TRIGGER IF NOT EXISTS account_removal_gates_require_live_account BEFORE INSERT ON account_removal_gates WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS messages_require_account BEFORE INSERT ON messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_sync_state_require_account BEFORE INSERT ON mailbox_sync_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_action_tombstones_require_account BEFORE INSERT ON mailbox_action_tombstones WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_catalog_state_require_account BEFORE INSERT ON mailbox_catalog_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_generations_require_account BEFORE INSERT ON mailbox_snapshot_generations WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_items_require_account BEFORE INSERT ON mailbox_snapshot_items WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_messages_require_account BEFORE INSERT ON mailbox_snapshot_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_snapshot_replacement_outcomes_require_account BEFORE INSERT ON mailbox_snapshot_replacement_outcomes WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_sync_failures_require_account BEFORE INSERT ON mailbox_sync_failures WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mail_rebuild_jobs_require_account BEFORE INSERT ON mail_rebuild_jobs WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS sync_runs_require_account BEFORE INSERT ON sync_runs WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS folder_sync_state_require_account BEFORE INSERT ON folder_sync_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS folder_sync_discovery_require_account BEFORE INSERT ON folder_sync_discovery WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_mutation_fences_require_account BEFORE INSERT ON mailbox_mutation_fences WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS operation_journal_require_account BEFORE INSERT ON operation_journal WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS gmail_logical_messages_require_account BEFORE INSERT ON gmail_logical_messages WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS gmail_message_memberships_require_account BEFORE INSERT ON gmail_message_memberships WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS gmail_label_reconciliation_state_require_account BEFORE INSERT ON gmail_label_reconciliation_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS gmail_inbox_epoch_after_insert AFTER INSERT ON messages WHEN NEW.mailbox = 'INBOX' AND EXISTS (SELECT 1 FROM accounts WHERE id = NEW.account_id) BEGIN INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (NEW.account_id, 1, CURRENT_TIMESTAMP) ON CONFLICT(account_id) DO UPDATE SET epoch = epoch + 1, updated_at = CURRENT_TIMESTAMP; END",
            "CREATE TRIGGER IF NOT EXISTS gmail_inbox_epoch_after_delete AFTER DELETE ON messages WHEN OLD.mailbox = 'INBOX' AND EXISTS (SELECT 1 FROM accounts WHERE id = OLD.account_id) BEGIN INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (OLD.account_id, 1, CURRENT_TIMESTAMP) ON CONFLICT(account_id) DO UPDATE SET epoch = epoch + 1, updated_at = CURRENT_TIMESTAMP; END",
            "CREATE TRIGGER IF NOT EXISTS gmail_inbox_epoch_after_update AFTER UPDATE OF mailbox ON messages WHEN OLD.mailbox IS NOT NEW.mailbox AND (OLD.mailbox = 'INBOX' OR NEW.mailbox = 'INBOX') AND EXISTS (SELECT 1 FROM accounts WHERE id = NEW.account_id) BEGIN INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (NEW.account_id, 1, CURRENT_TIMESTAMP) ON CONFLICT(account_id) DO UPDATE SET epoch = epoch + 1, updated_at = CURRENT_TIMESTAMP; END",
            "CREATE TRIGGER IF NOT EXISTS message_temporal_observations_require_account BEFORE INSERT ON message_temporal_observations WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS sent_correspondents_require_account BEFORE INSERT ON sent_correspondents WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
        ] {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("account deletion guard migration failed: {statement}"))?;
        }
        // `CREATE TRIGGER IF NOT EXISTS` cannot revise a trigger installed by
        // an earlier desktop version. Restrict epoch changes to actual Inbox
        // membership transitions, not ordinary flag/content updates.
        sqlx::query("DROP TRIGGER IF EXISTS gmail_inbox_epoch_after_update")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE TRIGGER gmail_inbox_epoch_after_update AFTER UPDATE OF mailbox ON messages WHEN OLD.mailbox IS NOT NEW.mailbox AND (OLD.mailbox = 'INBOX' OR NEW.mailbox = 'INBOX') AND EXISTS (SELECT 1 FROM accounts WHERE id = NEW.account_id) BEGIN INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (NEW.account_id, 1, CURRENT_TIMESTAMP) ON CONFLICT(account_id) DO UPDATE SET epoch = epoch + 1, updated_at = CURRENT_TIMESTAMP; END")
            .execute(&self.pool)
            .await?;
        let removal_gate_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(account_removal_gates)")
                .fetch_all(&self.pool)
                .await?;
        if !removal_gate_columns
            .iter()
            .any(|column| column.1 == "expires_at")
        {
            sqlx::query("ALTER TABLE account_removal_gates ADD COLUMN expires_at TEXT")
                .execute(&self.pool)
                .await?;
            // Existing gates predate the lease protocol. Treat them as
            // expired so an application upgrade cannot strand a live account
            // after a prior process crashed during account removal.
            sqlx::query(
                "UPDATE account_removal_gates SET expires_at = blocked_at WHERE expires_at IS NULL",
            )
            .execute(&self.pool)
            .await?;
        }
        if !removal_gate_columns
            .iter()
            .any(|column| column.1 == "owner")
        {
            sqlx::query(
                "ALTER TABLE account_removal_gates ADD COLUMN owner TEXT NOT NULL DEFAULT ''",
            )
            .execute(&self.pool)
            .await?;
        }
        let gmail_cursor_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(gmail_label_reconciliation_state)")
                .fetch_all(&self.pool)
                .await?;
        if !gmail_cursor_columns.iter().any(|column| column.1 == "span") {
            sqlx::query("ALTER TABLE gmail_label_reconciliation_state ADD COLUMN span INTEGER NOT NULL DEFAULT 64")
                .execute(&self.pool)
                .await?;
        }
        let journal_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(operation_journal)")
                .fetch_all(&self.pool)
                .await?;
        if !journal_columns
            .iter()
            .any(|column| column.1 == "smtp_accepted_at")
        {
            sqlx::query("ALTER TABLE operation_journal ADD COLUMN smtp_accepted_at TEXT")
                .execute(&self.pool)
                .await?;
        }
        for (table, replacement) in [
            (
                "gmail_message_memberships",
                "CREATE TABLE gmail_message_memberships_rebuilt (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, gmail_message_id TEXT NOT NULL, PRIMARY KEY(account_id, message_id), FOREIGN KEY(account_id, gmail_message_id) REFERENCES gmail_logical_messages(account_id, gmail_message_id) ON DELETE CASCADE)",
            ),
            (
                "message_temporal_observations",
                "CREATE TABLE message_temporal_observations_rebuilt (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, internal_date TEXT NOT NULL, message_date TEXT, observed_at TEXT NOT NULL, PRIMARY KEY(account_id, message_id))",
            ),
        ] {
            let schema: Option<String> = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_optional(&self.pool)
            .await?;
            if schema.is_some_and(|value| !value.to_ascii_uppercase().contains("ON UPDATE CASCADE")) {
                let rebuilt = format!("{table}_rebuilt");
                let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
                sqlx::query(replacement).execute(&mut *tx).await?;
                sqlx::query(&format!("INSERT INTO {rebuilt} SELECT * FROM {table}"))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(&format!("DROP TABLE {table}"))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(&format!("ALTER TABLE {rebuilt} RENAME TO {table}"))
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
        }
        // Rebuilding legacy child tables drops their table-owned guards.
        for statement in [
            "CREATE TRIGGER IF NOT EXISTS gmail_message_memberships_require_account BEFORE INSERT ON gmail_message_memberships WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS message_temporal_observations_require_account BEFORE INSERT ON message_temporal_observations WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
        ] {
            sqlx::query(statement).execute(&self.pool).await?;
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
        let account_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(accounts)")
                .fetch_all(&self.pool)
                .await?;
        if !account_columns
            .iter()
            .any(|column| column.1 == "config_generation")
        {
            sqlx::query(
                "ALTER TABLE accounts ADD COLUMN config_generation INTEGER NOT NULL DEFAULT 1",
            )
            .execute(&self.pool)
            .await?;
        }
        if !account_columns
            .iter()
            .any(|column| column.1 == "config_fingerprint")
        {
            sqlx::query(
                "ALTER TABLE accounts ADD COLUMN config_fingerprint TEXT NOT NULL DEFAULT ''",
            )
            .execute(&self.pool)
            .await?;
        }
        let accounts_without_fingerprint: Vec<(String, String)> =
            sqlx::query_as("SELECT id, data FROM accounts WHERE config_fingerprint = ''")
                .fetch_all(&self.pool)
                .await?;
        for (id, data) in accounts_without_fingerprint {
            // Older cache-only fixtures can contain a skeletal `{}` account
            // row. It has never represented a usable provider connection, so
            // leave it unfingerprinted rather than making database migration
            // fail before its stale content-fetch lease can be recovered.
            if let Ok(account) = deserialize_account(&data) {
                sqlx::query("UPDATE accounts SET config_fingerprint = ? WHERE id = ?")
                    .bind(account_provider_config_fingerprint(&account)?)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }
        // Legacy catalogues were written before the provider-generation
        // fence existed. Do this only after upgrading the accounts table,
        // because old desktop databases lack these account columns.
        if added_catalog_provider_stamp {
            sqlx::query("UPDATE mailbox_catalog_state SET provider_config_generation = (SELECT config_generation FROM accounts WHERE accounts.id = mailbox_catalog_state.account_id), provider_config_fingerprint = (SELECT config_fingerprint FROM accounts WHERE accounts.id = mailbox_catalog_state.account_id) WHERE provider_config_generation IS NULL AND EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_catalog_state.account_id)")
                .execute(&self.pool)
                .await?;
        }
        for table in ["folder_sync_state", "mailbox_snapshot_generations"] {
            let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
                sqlx::query_as(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&self.pool)
                    .await?;
            if !columns
                .iter()
                .any(|column| column.1 == "account_config_generation")
            {
                sqlx::query(&format!(
                    "ALTER TABLE {table} ADD COLUMN account_config_generation INTEGER"
                ))
                .execute(&self.pool)
                .await?;
            }
            if !columns
                .iter()
                .any(|column| column.1 == "account_config_fingerprint")
            {
                sqlx::query(&format!(
                    "ALTER TABLE {table} ADD COLUMN account_config_fingerprint TEXT"
                ))
                .execute(&self.pool)
                .await?;
            }
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
        let failure_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(mailbox_sync_failures)")
                .fetch_all(&self.pool)
                .await?;
        for (name, definition) in [
            ("error_class", "TEXT NOT NULL DEFAULT 'temporary'"),
            ("attempt_count", "INTEGER NOT NULL DEFAULT 0"),
            ("next_retry_at", "TEXT"),
            ("user_action_required", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !failure_columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE mailbox_sync_failures ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        for (table, name, definition) in [
            ("mailbox_mutation_fences", "uid_validity", "INTEGER"),
            (
                "mailbox_snapshot_items",
                "local_mutation_version",
                "INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
                sqlx::query_as(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&self.pool)
                    .await?;
            if !columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE {table} ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        let operation_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(operation_journal)")
                .fetch_all(&self.pool)
                .await?;
        for (name, definition) in [
            ("claim_owner", "TEXT"),
            ("claimed_at", "TEXT"),
            ("claimed_from_state", "TEXT"),
        ] {
            if !operation_columns.iter().any(|column| column.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE operation_journal ADD COLUMN {name} {definition}"
                ))
                .execute(&self.pool)
                .await?;
            }
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

    /// Legacy attachment rows only record MIME transport disposition. Rather
    /// than guessing whether a CID/logo is a user-facing file, discard stale
    /// attachment metadata and let the next authoritative MIME fetch classify
    /// the selected HTML branch. This also prevents stale paperclips from
    /// surviving the schema upgrade.
    async fn migrate_attachment_presentation_metadata(&self) -> Result<()> {
        for table in ["attachments", "starred_attachment_metadata"] {
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
                 UNION SELECT account_id FROM mailbox_catalog_state \
                 UNION SELECT account_id FROM mailbox_snapshot_generations \
                 UNION SELECT account_id FROM mailbox_sync_failures \
                 UNION SELECT account_id FROM mailbox_action_tombstones \
                 UNION SELECT account_id FROM mail_rebuild_jobs \
                 UNION SELECT account_id FROM sent_correspondents \
             ) AS orphan \
             WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE id = orphan.account_id)",
        )
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;
        // Delete message dependents explicitly before their parent. Modern
        // schema revisions also cascade these rows, but explicit cleanup
        // repairs older local schemas that may not have had those FKs.
        for statement in [
            "DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id))",
            "DELETE FROM messages WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = messages.account_id)",
            "DELETE FROM mailbox_sync_state WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_sync_state.account_id)",
            "DELETE FROM mailbox_catalog_state WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_catalog_state.account_id)",
            "DELETE FROM mailbox_snapshot_generations WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_snapshot_generations.account_id)",
            "DELETE FROM mailbox_sync_failures WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_sync_failures.account_id)",
            "DELETE FROM mailbox_action_tombstones WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_action_tombstones.account_id)",
            "DELETE FROM mail_rebuild_jobs WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mail_rebuild_jobs.account_id)",
            "DELETE FROM sent_correspondents WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = sent_correspondents.account_id)",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
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
            sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?, NULL, ?, ?, '', '', '', ?, ?, '', NULL, 'headers_only', NULL, NULL, 0, ?, ?, 0, NULL, NULL, NULL, '')")
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

    /// Reads endpoint configuration and its credential from one SQLite
    /// snapshot. An old provider worker must never authenticate to an old
    /// endpoint using a password saved for newly configured account settings.
    pub async fn secret_for_account(
        &self,
        account: &Account,
        name: &str,
    ) -> Result<Option<String>> {
        let row: Option<(String, Option<Vec<u8>>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT account.data, credential.nonce, credential.ciphertext FROM accounts AS account LEFT JOIN credentials AS credential ON credential.name = ? WHERE account.id = ? AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS gate WHERE gate.account_id = account.id AND gate.expires_at > ?)",
        ).bind(name).bind(account.id.to_string()).bind(Utc::now()).fetch_optional(&self.pool).await?;
        let Some((data, nonce, ciphertext)) = row else {
            return Err(anyhow!(
                "Account settings changed; reload the account before trying again"
            ));
        };
        let current = deserialize_account(&data)?;
        let mut expected = account.clone();
        expected.ensure_account_name();
        if serde_json::to_value(current)? != serde_json::to_value(expected)? {
            return Err(anyhow!(
                "Account settings changed; reload the account before trying again"
            ));
        }
        match (nonce, ciphertext) {
            (Some(nonce), Some(ciphertext)) => {
                decrypt_secret(&self.vault_key, &nonce, name, ciphertext).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Publishes a refresh only if both the account settings and the exact
    /// token snapshot used for that refresh still match. A concurrent account
    /// update or another process's token rotation cannot be overwritten.
    pub async fn acquire_oauth_refresh_lease(
        &self,
        account: &Account,
        name: &str,
    ) -> Result<Option<OAuthRefreshLease>> {
        let owner = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let claimed = sqlx::query("INSERT INTO oauth_refresh_leases(secret_name, owner, expires_at) SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?) AND EXISTS (SELECT 1 FROM credentials WHERE name = ?) AND NOT EXISTS (SELECT 1 FROM account_removal_gates WHERE account_id = ? AND expires_at > ?) ON CONFLICT(secret_name) DO UPDATE SET owner = excluded.owner, expires_at = excluded.expires_at WHERE oauth_refresh_leases.expires_at <= ?")
            .bind(name).bind(&owner).bind(now + chrono::Duration::seconds(120))
            .bind(account.id.to_string()).bind(name).bind(account.id.to_string()).bind(now).bind(now)
            .execute(&self.pool).await?.rows_affected() == 1;
        if !claimed {
            return Ok(None);
        }
        let (store, renewal_name, renewal_owner) = (self.clone(), name.to_owned(), owner.clone());
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let renewed = sqlx::query("UPDATE oauth_refresh_leases SET expires_at = ? WHERE secret_name = ? AND owner = ?")
                    .bind(Utc::now() + chrono::Duration::seconds(120)).bind(&renewal_name).bind(&renewal_owner).execute(&store.pool).await;
                if !matches!(renewed, Ok(result) if result.rows_affected() == 1) {
                    break;
                }
            }
        });
        Ok(Some(OAuthRefreshLease {
            store: self.clone(),
            secret_name: name.to_owned(),
            owner,
            task,
        }))
    }

    pub async fn replace_oauth_secret_for_account(
        &self,
        account: &Account,
        name: &str,
        expected_secret: &str,
        replacement: &str,
        lease: &OAuthRefreshLease,
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owns_lease: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_refresh_leases WHERE secret_name = ? AND owner = ? AND expires_at > ?)")
            .bind(name).bind(&lease.owner).bind(Utc::now()).fetch_one(&mut *tx).await?;
        if lease.secret_name != name || !owns_lease {
            tx.rollback().await?;
            return Ok(false);
        }
        let row: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT account.data, credential.nonce, credential.ciphertext FROM accounts AS account JOIN credentials AS credential ON credential.name = ? WHERE account.id = ? AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS gate WHERE gate.account_id = account.id AND gate.expires_at > ?)",
        ).bind(name).bind(account.id.to_string()).bind(Utc::now()).fetch_optional(&mut *tx).await?;
        let Some((data, nonce, ciphertext)) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        let current = deserialize_account(&data)?;
        let mut expected = account.clone();
        expected.ensure_account_name();
        if !matches!(current.auth, AccountAuth::OAuth2 { .. })
            || serde_json::to_value(current)? != serde_json::to_value(expected)?
            || decrypt_secret(&self.vault_key, &nonce, name, ciphertext)? != expected_secret
        {
            tx.rollback().await?;
            return Ok(false);
        }
        let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
        let ciphertext = encrypt_secret(&self.vault_key, nonce, name, replacement)?;
        sqlx::query(
            "UPDATE credentials SET nonce = ?, ciphertext = ?, updated_at = ? WHERE name = ?",
        )
        .bind(nonce.as_slice())
        .bind(ciphertext)
        .bind(Utc::now())
        .bind(name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
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
        save_account_in_transaction(&mut tx, account).await?;
        tx.commit().await?;
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
        save_account_in_transaction(&mut tx, account).await?;
        save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        tx.commit().await?;
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
        save_account_in_transaction(&mut tx, account).await?;
        save_mail_rebuild_job_in_transaction(&mut tx, job).await?;
        if let Some(previous_secret_name) = previous_secret_name {
            sqlx::query("DELETE FROM credentials WHERE name = ?")
                .bind(previous_secret_name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
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
        let deleted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(account.id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if deleted {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(name) DO UPDATE SET nonce=excluded.nonce, ciphertext=excluded.ciphertext, updated_at=excluded.updated_at")
            .bind(secret_name)
            .bind(nonce.as_slice())
            .bind(ciphertext)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        save_account_in_transaction(&mut tx, account).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Creates a newly authenticated account and its durable initial sync.
    /// Authentication is performed before calling this method. The account,
    /// encrypted credential and restartable Inbox intent become visible in
    /// one commit, so a crash cannot leave an account without its initial job.
    pub async fn create_account_with_secret_and_initial_sync(
        &self,
        account: &Account,
        secret_name: &str,
        secret: &str,
    ) -> Result<SyncRun> {
        let nonce = random_bytes::<VAULT_NONCE_LEN>()?;
        let ciphertext = encrypt_secret(&self.vault_key, nonce, secret_name, secret)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? OR LOWER(TRIM(email)) = LOWER(TRIM(?)))")
            .bind(account.id.to_string()).bind(&account.email).fetch_one(&mut *tx).await?;
        if exists {
            tx.rollback().await?;
            return Err(anyhow!("This email account is already connected"));
        }
        save_account_in_transaction(&mut tx, account).await?;
        sqlx::query(
            "INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(secret_name)
        .bind(nonce.as_slice())
        .bind(ciphertext)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        sqlx::query("INSERT INTO sync_runs(run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, content_loading, retry_count, outcome, revision, next_retry_at, error, created_at, updated_at) VALUES (?, ?, 'initial_inbox', 0, 0, 0, 0, 0, 0, 'running', 0, NULL, NULL, ?, ?)")
            .bind(&run_id).bind(account.id.to_string()).bind(now).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        self.sync_run_by_id(&run_id)
            .await?
            .ok_or_else(|| anyhow!("created initial sync run is missing"))
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

    /// Starts one backend-owned sync run, or returns the interrupted active
    /// run for the account so an app restart resumes rather than duplicates
    /// scheduling work.
    pub async fn create_sync_run(&self, account_id: AccountId) -> Result<SyncRun> {
        let account_id_text = account_id.to_string();
        let run_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<String> = sqlx::query_scalar("SELECT run_id FROM sync_runs WHERE account_id = ? AND outcome = 'running' ORDER BY revision DESC LIMIT 1")
            .bind(&account_id_text).fetch_optional(&mut *tx).await?;
        if let Some(active) = active {
            tx.commit().await?;
            return self
                .sync_run_by_id(&active)
                .await?
                .ok_or_else(|| anyhow!("active sync run is missing"));
        }
        let revision: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision), -1) + 1 FROM sync_runs WHERE account_id = ?",
        )
        .bind(&account_id_text)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO sync_runs(run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, content_loading, retry_count, outcome, revision, next_retry_at, error, created_at, updated_at) VALUES (?, ?, 'initial_inbox', 0, 0, 0, 0, 0, 0, 'running', ?, NULL, NULL, ?, ?)")
            .bind(&run_id)
            .bind(&account_id_text)
            .bind(revision)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.sync_run_by_id(&run_id)
            .await?
            .ok_or_else(|| anyhow!("created sync run is missing"))
    }

    /// Returns the active run when present, otherwise the most recently
    /// updated terminal run. The backend can therefore restore status after a
    /// missed publication event.
    pub async fn sync_run(&self, account_id: AccountId) -> Result<Option<SyncRun>> {
        let row: Option<SyncRunRow> = sqlx::query_as("SELECT run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, EXISTS(SELECT 1 FROM message_content_fetches AS fetch JOIN messages AS message ON message.id = fetch.message_id WHERE message.account_id = sync_runs.account_id AND fetch.claimed_at > ?) AS content_loading, retry_count, outcome, revision, next_retry_at, error FROM sync_runs WHERE account_id = ? ORDER BY CASE outcome WHEN 'running' THEN 0 ELSE 1 END, updated_at DESC LIMIT 1")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS))
            .bind(account_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(sync_run_from_row).transpose()
    }

    pub async fn sync_runs(&self) -> Result<Vec<SyncRun>> {
        let rows: Vec<SyncRunRow> = sqlx::query_as("SELECT run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, EXISTS(SELECT 1 FROM message_content_fetches AS fetch JOIN messages AS message ON message.id = fetch.message_id WHERE message.account_id = ranked.account_id AND fetch.claimed_at > ?) AS content_loading, retry_count, outcome, revision, next_retry_at, error FROM (SELECT run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, retry_count, outcome, revision, next_retry_at, error, ROW_NUMBER() OVER (PARTITION BY account_id ORDER BY revision DESC, updated_at DESC, run_id DESC) AS rank FROM sync_runs) AS ranked WHERE rank = 1 ORDER BY account_id")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(sync_run_from_row).collect()
    }

    /// Converts retained pre-run rebuild checkpoints once. The old row has no
    /// safe per-folder cursor, so the new run restarts at its first bounded
    /// Inbox stage while preserving the already committed catalogue instead
    /// of invoking the destructive legacy reset path.
    pub async fn migrate_mail_rebuild_jobs_to_sync_runs(&self) -> Result<Vec<SyncRun>> {
        let jobs = self.mail_rebuild_jobs().await?;
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now();
        for job in &jobs {
            let run_id = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT OR IGNORE INTO sync_runs(run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, content_loading, retry_count, outcome, revision, next_retry_at, error, created_at, updated_at) VALUES (?, ?, 'initial_inbox', 0, 0, 0, 0, 0, 0, 'running', 0, NULL, ?, ?, ?)")
                .bind(run_id).bind(job.account_id.to_string()).bind(format!("migrated legacy rebuild checkpoint: {}", job.phase)).bind(now).bind(now)
                .execute(&mut *tx).await?;
            sqlx::query("DELETE FROM mail_rebuild_jobs WHERE account_id = ?")
                .bind(job.account_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let migrated = self.sync_runs().await?;
        let account_ids: HashSet<AccountId> = jobs.into_iter().map(|job| job.account_id).collect();
        Ok(migrated
            .into_iter()
            .filter(|run| account_ids.contains(&run.account_id) && run.outcome == "running")
            .collect())
    }

    pub async fn update_sync_run(
        &self,
        run_id: &str,
        update: &SyncRunUpdate<'_>,
    ) -> Result<SyncRun> {
        let retry_after_supplied = update.next_retry_at.is_some();
        let next_retry_at = update.next_retry_at.flatten();
        let error_supplied = update.error.is_some();
        let error = update.error.flatten();
        let changed = sqlx::query("UPDATE sync_runs SET stage = COALESCE(?, stage), inbox_ready = COALESCE(?, inbox_ready), primary_complete = COALESCE(?, primary_complete), secondary_complete = COALESCE(?, secondary_complete), deferred_complete = COALESCE(?, deferred_complete), content_loading = COALESCE(?, content_loading), retry_count = COALESCE(?, retry_count), outcome = COALESCE(?, outcome), next_retry_at = CASE WHEN ? THEN ? ELSE next_retry_at END, error = CASE WHEN ? THEN ? ELSE error END, revision = revision + 1, updated_at = ? WHERE run_id = ? AND outcome = 'running'")
            .bind(update.stage)
            .bind(update.inbox_ready)
            .bind(update.primary_complete)
            .bind(update.secondary_complete)
            .bind(update.deferred_complete)
            .bind(update.content_loading)
            .bind(update.retry_count.map(i64::from))
            .bind(update.outcome)
            .bind(retry_after_supplied)
            .bind(next_retry_at)
            .bind(error_supplied)
            .bind(error)
            .bind(Utc::now())
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(anyhow!("sync run is not active"));
        }
        self.sync_run_by_id(run_id)
            .await?
            .ok_or_else(|| anyhow!("updated sync run is missing"))
    }

    async fn sync_run_by_id(&self, run_id: &str) -> Result<Option<SyncRun>> {
        let row: Option<SyncRunRow> = sqlx::query_as("SELECT run_id, account_id, stage, inbox_ready, primary_complete, secondary_complete, deferred_complete, content_loading, retry_count, outcome, revision, next_retry_at, error FROM sync_runs WHERE run_id = ?")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(sync_run_from_row).transpose()
    }

    /// Starts or resumes a folder scan. A UIDVALIDITY mismatch replaces only
    /// temporary discovery state: committed mail remains readable until the
    /// replacement snapshot is proven complete.
    pub async fn begin_folder_sync(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        upper_boundary: Option<u32>,
    ) -> Result<FolderSyncState> {
        self.begin_folder_sync_inner(
            account_id,
            mailbox,
            remote_name,
            uid_validity,
            upper_boundary,
            None,
        )
        .await
    }

    /// Captures the exact account configuration before folder-provider I/O.
    /// All later publications of this generation are rejected if that account
    /// changes endpoint, principal, or provider configuration.
    pub async fn begin_folder_sync_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        upper_boundary: Option<u32>,
    ) -> Result<FolderSyncState> {
        let receipt = self
            .capture_account_provider_receipt(account)
            .await?
            .ok_or_else(|| anyhow!("account provider configuration is no longer current"))?;
        self.begin_folder_sync_with_provider_receipt(
            &receipt,
            mailbox,
            remote_name,
            uid_validity,
            upper_boundary,
        )
        .await
    }

    /// Begins a folder generation using a receipt captured before IMAP I/O.
    pub async fn begin_folder_sync_with_provider_receipt(
        &self,
        receipt: &AccountProviderReceipt,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        upper_boundary: Option<u32>,
    ) -> Result<FolderSyncState> {
        self.begin_folder_sync_inner(
            receipt.account_id,
            mailbox,
            remote_name,
            uid_validity,
            upper_boundary,
            Some(receipt),
        )
        .await
    }

    async fn begin_folder_sync_inner(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        upper_boundary: Option<u32>,
        receipt: Option<&AccountProviderReceipt>,
    ) -> Result<FolderSyncState> {
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(receipt) = receipt {
            ensure_account_provider_receipt_current_in_transaction(&mut tx, receipt).await?;
        }
        let existing: Option<FolderSyncState> = sqlx::query_as("SELECT account_id, mailbox, remote_name, uid_validity, generation, upper_boundary, cursor, discovery_complete, headers_complete, local_mutation_version, revision, retry_after, account_config_generation, account_config_fingerprint, updated_at FROM folder_sync_state WHERE account_id = ? AND mailbox = ?")
            .bind(&account_id)
            .bind(mailbox)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(existing) = existing {
            if existing.remote_name == remote_name
                && existing.uid_validity == i64::from(uid_validity)
                && receipt.is_none_or(|receipt| {
                    folder_sync_state_matches_provider_receipt(&existing, receipt)
                })
            {
                tx.commit().await?;
                return Ok(existing);
            }
        }
        let generation = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        sqlx::query("INSERT INTO folder_sync_state(account_id, mailbox, remote_name, uid_validity, generation, upper_boundary, cursor, discovery_complete, headers_complete, local_mutation_version, revision, retry_after, account_config_generation, account_config_fingerprint, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, 0, 0, 0, 0, NULL, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, generation=excluded.generation, upper_boundary=excluded.upper_boundary, cursor=NULL, discovery_complete=0, headers_complete=0, revision=folder_sync_state.revision+1, retry_after=NULL, account_config_generation=excluded.account_config_generation, account_config_fingerprint=excluded.account_config_fingerprint, updated_at=excluded.updated_at")
            .bind(&account_id)
            .bind(mailbox)
            .bind(remote_name)
            .bind(i64::from(uid_validity))
            .bind(&generation)
            .bind(upper_boundary.map(i64::from))
            .bind(receipt.map(|receipt| receipt.config_generation))
            .bind(receipt.map(|receipt| receipt.config_fingerprint.as_str()))
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let state: FolderSyncState = sqlx::query_as("SELECT account_id, mailbox, remote_name, uid_validity, generation, upper_boundary, cursor, discovery_complete, headers_complete, local_mutation_version, revision, retry_after, account_config_generation, account_config_fingerprint, updated_at FROM folder_sync_state WHERE account_id = ? AND mailbox = ?")
            .bind(&account_id)
            .bind(mailbox)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(state)
    }

    pub async fn folder_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Option<FolderSyncState>> {
        Ok(sqlx::query_as("SELECT account_id, mailbox, remote_name, uid_validity, generation, upper_boundary, cursor, discovery_complete, headers_complete, local_mutation_version, revision, retry_after, account_config_generation, account_config_fingerprint, updated_at FROM folder_sync_state WHERE account_id = ? AND mailbox = ?")
            .bind(account_id.to_string())
            .bind(mailbox)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Commits one complete discovery response. The cursor is not advanced on
    /// an interrupted response because callers invoke this only after tagged
    /// OK, keeping the temporary UID evidence resumable and bounded.
    pub async fn stage_folder_discovery_page(
        &self,
        account_id: AccountId,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        uids: &[u32],
        next_cursor: Option<u32>,
        complete: bool,
    ) -> Result<u64> {
        if uids.contains(&0) {
            return Err(anyhow!("folder discovery contains UID 0"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let publication_timer = PublicationTransactionTimer::start();
        let state: Option<(String, Option<i64>, i64, Option<i64>, Option<String>)> = sqlx::query_as("SELECT generation, upper_boundary, revision, account_config_generation, account_config_fingerprint FROM folder_sync_state WHERE account_id = ? AND mailbox = ? AND generation = ? AND revision = ?")
            .bind(&account_id)
            .bind(mailbox)
            .bind(expected_generation)
            .bind(i64::try_from(expected_revision)?)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((generation, upper_boundary, revision, config_generation, config_fingerprint)) =
            state
        else {
            tx.rollback().await?;
            return Err(anyhow!("folder sync state is not active"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
        if uids
            .iter()
            .any(|uid| upper_boundary.is_some_and(|boundary| i64::from(*uid) > boundary))
        {
            tx.rollback().await?;
            return Err(anyhow!(
                "folder discovery exceeds its captured UID boundary"
            ));
        }
        let now = Utc::now();
        for uid in uids {
            sqlx::query("INSERT OR IGNORE INTO folder_sync_discovery(account_id, mailbox, generation, uid, created_at) VALUES (?, ?, ?, ?, ?)")
                .bind(&account_id).bind(mailbox).bind(&generation).bind(i64::from(*uid)).bind(now)
                .execute(&mut *tx).await?;
        }
        let updated = sqlx::query("UPDATE folder_sync_state SET cursor = ?, discovery_complete = ?, revision = revision + 1, updated_at = ? WHERE account_id = ? AND mailbox = ? AND generation = ? AND revision = ?")
            .bind(next_cursor.map(i64::from)).bind(complete).bind(now).bind(&account_id).bind(mailbox).bind(&generation).bind(revision)
            .execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!("folder sync state changed while staging discovery"));
        }
        tx.commit().await?;
        publication_timer.committed();
        Ok(u64::try_from(revision + 1)?)
    }

    pub async fn folder_discovered_uids_needing_headers(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: usize,
    ) -> Result<Vec<u32>> {
        let limit = i64::try_from(limit.clamp(1, 500))?;
        let rows: Vec<i64> = sqlx::query_scalar("SELECT discovery.uid FROM folder_sync_discovery AS discovery JOIN folder_sync_state AS state ON state.account_id = discovery.account_id AND state.mailbox = discovery.mailbox AND state.generation = discovery.generation LEFT JOIN folder_sync_header_outcomes AS outcome ON outcome.account_id = discovery.account_id AND outcome.mailbox = discovery.mailbox AND outcome.generation = discovery.generation AND outcome.uid = discovery.uid LEFT JOIN mailbox_sync_failures AS failure ON failure.account_id = discovery.account_id AND failure.mailbox = discovery.mailbox AND failure.uid = discovery.uid AND failure.stage = 'headers' WHERE discovery.account_id = ? AND discovery.mailbox = ? AND outcome.uid IS NULL AND (failure.uid IS NULL OR (failure.user_action_required = 0 AND (failure.next_retry_at IS NULL OR failure.next_retry_at <= ?))) ORDER BY discovery.uid DESC LIMIT ?")
            .bind(account_id.to_string()).bind(mailbox).bind(Utc::now()).bind(limit).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|uid| u32::try_from(uid).context("stored discovered UID is invalid"))
            .collect()
    }

    /// Reports unresolved discovered headers even when their retry schedule
    /// deliberately keeps them out of the next fetch page. Callers must not
    /// mark a folder header pass complete merely because no UID is due now.
    pub async fn folder_has_unresolved_headers(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM folder_sync_discovery AS discovery JOIN folder_sync_state AS state ON state.account_id = discovery.account_id AND state.mailbox = discovery.mailbox AND state.generation = discovery.generation LEFT JOIN folder_sync_header_outcomes AS outcome ON outcome.account_id = discovery.account_id AND outcome.mailbox = discovery.mailbox AND outcome.generation = discovery.generation AND outcome.uid = discovery.uid WHERE discovery.account_id = ? AND discovery.mailbox = ? AND outcome.uid IS NULL)")
            .bind(account_id.to_string()).bind(mailbox).fetch_one(&self.pool).await?)
    }

    /// Publishes a parsed header page and advances the durable folder revision
    /// in the same transaction. The revision is the event fence callers use
    /// to ignore stale catalogue publications.
    pub async fn commit_folder_header_batch(
        &self,
        account_id: AccountId,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        messages: &[MailSummary],
        next_cursor: Option<u32>,
        headers_complete: bool,
    ) -> Result<u64> {
        self.commit_folder_header_batch_inner(
            account_id,
            mailbox,
            expected_generation,
            expected_revision,
            messages,
            next_cursor,
            headers_complete,
            None,
            None,
        )
        .await
    }

    /// Publishes an IMAP header response using the account receipt captured
    /// before that response was fetched.
    pub async fn commit_folder_header_batch_with_provider_receipt(
        &self,
        receipt: &AccountProviderReceipt,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        messages: &[MailSummary],
        next_cursor: Option<u32>,
        headers_complete: bool,
    ) -> Result<u64> {
        self.commit_folder_header_batch_inner(
            receipt.account_id,
            mailbox,
            expected_generation,
            expected_revision,
            messages,
            next_cursor,
            headers_complete,
            Some(receipt),
            None,
        )
        .await
    }

    /// Convenience form for code which retains the exact Account used for
    /// the provider request.
    pub async fn commit_folder_header_batch_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        messages: &[MailSummary],
        next_cursor: Option<u32>,
        headers_complete: bool,
    ) -> Result<u64> {
        let receipt = self
            .capture_account_provider_receipt(account)
            .await?
            .ok_or_else(|| anyhow!("account provider configuration is no longer current"))?;
        self.commit_folder_header_batch_with_provider_receipt(
            &receipt,
            mailbox,
            expected_generation,
            expected_revision,
            messages,
            next_cursor,
            headers_complete,
        )
        .await
    }

    /// Atomically publishes a folder header batch and its Gmail identities.
    /// The Gmail Inbox epoch is checked before any header insert, so these
    /// headers cannot invalidate their own pre-fetch receipt.
    pub async fn commit_folder_header_batch_with_gmail_observations(
        &self,
        receipt: &AccountProviderReceipt,
        gmail_receipt: &GmailInboxMembershipEpochReceipt,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        messages: &[MailSummary],
        observations: &[GmailProviderObservation],
        next_cursor: Option<u32>,
        headers_complete: bool,
    ) -> Result<u64> {
        if gmail_receipt.account_id != receipt.account_id {
            return Err(anyhow!("Gmail epoch receipt belongs to another account"));
        }
        let mut observed_uids = HashSet::new();
        let mut gmail_ids = HashSet::new();
        for observation in observations {
            if observation.uid == 0 || observation.gmail_message_id.trim().is_empty() {
                return Err(anyhow!("Gmail header observation is invalid"));
            }
            if !observed_uids.insert(observation.uid)
                || !gmail_ids.insert(observation.gmail_message_id.as_str())
            {
                return Err(anyhow!(
                    "a Gmail header batch cannot repeat a UID or Gmail identity"
                ));
            }
        }
        self.commit_folder_header_batch_inner(
            receipt.account_id,
            mailbox,
            expected_generation,
            expected_revision,
            messages,
            next_cursor,
            headers_complete,
            Some(receipt),
            Some((gmail_receipt, observations)),
        )
        .await
    }

    async fn commit_folder_header_batch_inner(
        &self,
        account_id: AccountId,
        mailbox: &str,
        expected_generation: &str,
        expected_revision: u64,
        messages: &[MailSummary],
        next_cursor: Option<u32>,
        headers_complete: bool,
        receipt: Option<&AccountProviderReceipt>,
        gmail_observations: Option<(
            &GmailInboxMembershipEpochReceipt,
            &[GmailProviderObservation],
        )>,
    ) -> Result<u64> {
        let account_id_text = account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_id_text || message.mailbox != mailbox)
        {
            return Err(anyhow!(
                "folder header batch message does not match its mailbox"
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let publication_timer = PublicationTransactionTimer::start();
        let state: Option<(String, Option<i64>, String, i64, Option<i64>, Option<String>)> = sqlx::query_as("SELECT generation, upper_boundary, remote_name, uid_validity, account_config_generation, account_config_fingerprint FROM folder_sync_state WHERE account_id = ? AND mailbox = ? AND generation = ? AND revision = ?")
            .bind(&account_id_text).bind(mailbox).bind(expected_generation).bind(i64::try_from(expected_revision)?)
            .fetch_optional(&mut *tx).await?;
        let Some((
            generation,
            upper_boundary,
            remote_name,
            uid_validity,
            config_generation,
            config_fingerprint,
        )) = state
        else {
            tx.rollback().await?;
            return Err(anyhow!("folder sync revision is stale"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id_text,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
        if let Some(receipt) = receipt {
            ensure_account_provider_receipt_current_in_transaction(&mut tx, receipt).await?;
            if receipt.account_id.to_string() != account_id_text
                || config_generation != Some(receipt.config_generation)
                || config_fingerprint.as_deref() != Some(receipt.config_fingerprint.as_str())
            {
                tx.rollback().await?;
                return Err(anyhow!(
                    "folder sync generation belongs to another provider configuration"
                ));
            }
        }
        if let Some((gmail_receipt, _)) = gmail_observations {
            let current_epoch: Option<i64> = sqlx::query_scalar(
                "SELECT epoch FROM gmail_inbox_membership_epochs WHERE account_id = ?",
            )
            .bind(&account_id_text)
            .fetch_optional(&mut *tx)
            .await?;
            if current_epoch.unwrap_or(0) != gmail_receipt.epoch {
                tx.rollback().await?;
                return Err(anyhow!(
                    "Gmail Inbox membership changed during header fetch"
                ));
            }
        }
        let committed_uid_validity: Option<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id_text)
        .bind(mailbox)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if committed_uid_validity.is_some_and(|current| current != uid_validity) {
            tx.rollback().await?;
            return Err(anyhow!(
                "folder generation UIDVALIDITY is older than the committed mailbox namespace"
            ));
        }
        if messages.iter().any(|message| {
            message.uid <= 0 || upper_boundary.is_some_and(|boundary| message.uid > boundary)
        }) {
            tx.rollback().await?;
            return Err(anyhow!(
                "folder header batch exceeds its captured UID boundary"
            ));
        }
        for message in messages {
            let discovered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM folder_sync_discovery WHERE account_id = ? AND mailbox = ? AND generation = ? AND uid = ?)")
                .bind(&account_id_text).bind(mailbox).bind(&generation).bind(message.uid)
                .fetch_one(&mut *tx).await?;
            if !discovered {
                tx.rollback().await?;
                return Err(anyhow!(
                    "header UID was not discovered in the active folder generation"
                ));
            }
            persist_message(&mut tx, message).await?;
            sqlx::query("INSERT INTO folder_sync_header_outcomes(account_id, mailbox, generation, uid, completed_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET completed_at=excluded.completed_at")
                .bind(&account_id_text).bind(mailbox).bind(&generation).bind(message.uid).bind(Utc::now())
                .execute(&mut *tx).await?;
        }
        if let Some((_gmail_receipt, observations)) = gmail_observations {
            let message_uids: HashSet<i64> = messages.iter().map(|message| message.uid).collect();
            if observations
                .iter()
                .any(|observation| !message_uids.contains(&i64::from(observation.uid)))
            {
                tx.rollback().await?;
                return Err(anyhow!(
                    "Gmail header observation does not match a published header"
                ));
            }
            let now = Utc::now();
            for observation in observations {
                let local_message_id: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
                )
                .bind(&account_id_text)
                .bind(mailbox)
                .bind(i64::from(observation.uid))
                .fetch_optional(&mut *tx)
                .await?;
                let Some(local_message_id) = local_message_id else {
                    tx.rollback().await?;
                    return Err(anyhow!(
                        "published Gmail header has no canonical local message"
                    ));
                };
                sqlx::query("INSERT INTO gmail_logical_messages(account_id, gmail_message_id, labels_json, observed_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, gmail_message_id) DO UPDATE SET labels_json=excluded.labels_json, observed_at=excluded.observed_at")
                    .bind(&account_id_text)
                    .bind(&observation.gmail_message_id)
                    .bind(serde_json::to_string(&observation.labels)?)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO gmail_message_memberships(account_id, message_id, gmail_message_id) VALUES (?, ?, ?) ON CONFLICT(account_id, message_id) DO UPDATE SET gmail_message_id=excluded.gmail_message_id")
                    .bind(&account_id_text)
                    .bind(&local_message_id)
                    .bind(&observation.gmail_message_id)
                    .execute(&mut *tx)
                    .await?;
            }
            for observation in observations {
                if observation
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("\\Inbox"))
                {
                    continue;
                }
                let ids: Vec<String> = sqlx::query_scalar("SELECT message.id FROM gmail_message_memberships AS membership JOIN messages AS message ON message.id = membership.message_id WHERE membership.account_id = ? AND membership.gmail_message_id = ? AND message.mailbox = 'INBOX'")
                    .bind(&account_id_text)
                    .bind(&observation.gmail_message_id)
                    .fetch_all(&mut *tx)
                    .await?;
                for id in ids {
                    sqlx::query("DELETE FROM messages WHERE id = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            // Inbox insert/delete triggers may already advance the epoch. An
            // explicit bump also fences a labels-only observation batch.
            sqlx::query("INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (?, 1, ?) ON CONFLICT(account_id) DO UPDATE SET epoch=epoch+1, updated_at=excluded.updated_at")
                .bind(&account_id_text)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        // First headers make a folder usable immediately. Historical coverage
        // remains false until discovery and retry queues complete, but remote
        // actions can resolve this committed locator without a separate write.
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, provider_config_generation, provider_config_fingerprint, updated_at) VALUES (?, ?, ?, ?, 0, 0, NULL, NULL, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, historical_complete=0, provider_config_generation=excluded.provider_config_generation, provider_config_fingerprint=excluded.provider_config_fingerprint, updated_at=excluded.updated_at")
            .bind(&account_id_text).bind(mailbox).bind(&remote_name).bind(uid_validity).bind(config_generation).bind(&config_fingerprint).bind(Utc::now())
            .execute(&mut *tx).await?;
        let updated = sqlx::query("UPDATE folder_sync_state SET cursor = ?, headers_complete = ?, revision = revision + 1, updated_at = ? WHERE account_id = ? AND mailbox = ? AND generation = ? AND revision = ?")
            .bind(next_cursor.map(i64::from)).bind(headers_complete).bind(Utc::now()).bind(&account_id_text).bind(mailbox).bind(&generation).bind(i64::try_from(expected_revision)?)
            .execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!(
                "folder sync state changed while publishing headers"
            ));
        }
        tx.commit().await?;
        publication_timer.committed();
        Ok(expected_revision + 1)
    }

    /// Atomically creates the durable record which authorizes an optimistic
    /// local mutation. Remote workers must use this immutable locator rather
    /// than whichever row happens to be selected when they eventually run.
    pub async fn enqueue_operation(
        &self,
        account_id: AccountId,
        kind: &str,
        target: OperationTarget<'_>,
        payload_json: &str,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        if kind.trim().is_empty() {
            return Err(anyhow!("operation kind is required"));
        }
        serde_json::from_str::<serde_json::Value>(payload_json)
            .context("operation payload must be valid JSON")?;
        if target.uid == Some(0)
            || target.mailbox.is_some() != target.uid.is_some()
            || target.uid_validity.is_some() != target.uid.is_some()
        {
            return Err(anyhow!(
                "operation target has an invalid mailbox UID locator"
            ));
        }
        let account_id_text = account_id.to_string();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?) AND NOT EXISTS (SELECT 1 FROM account_removal_gates WHERE account_id = ? AND expires_at > ?))")
            .bind(&account_id_text).bind(&account_id_text).bind(&account_id_text).bind(Utc::now()).fetch_one(&mut *tx).await?;
        if !account_live {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        let local_version = if let (Some(mailbox), Some(uid)) = (target.mailbox, target.uid) {
            sqlx::query("UPDATE folder_sync_state SET local_mutation_version = local_mutation_version + 1, revision = revision + 1, updated_at = ? WHERE account_id = ? AND mailbox = ?")
                .bind(Utc::now()).bind(&account_id_text).bind(mailbox).execute(&mut *tx).await?;
            let uid_validity = target
                .uid_validity
                .map(|value| i64::try_from(value))
                .transpose()?;
            let version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) + 1 FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity IS ?")
                .bind(&account_id_text).bind(mailbox).bind(i64::from(uid)).bind(uid_validity).fetch_one(&mut *tx).await?;
            if let Some(uid_validity) = uid_validity {
                sqlx::query("INSERT INTO mailbox_mutation_versions(account_id, mailbox, uid, uid_validity, version, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid, uid_validity) DO UPDATE SET version=excluded.version, updated_at=excluded.updated_at")
                    .bind(&account_id_text).bind(mailbox).bind(i64::from(uid)).bind(uid_validity).bind(version).bind(Utc::now()).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO mailbox_mutation_fences(account_id, mailbox, uid, uid_validity, version, operation_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET uid_validity=excluded.uid_validity, version=excluded.version, operation_id=excluded.operation_id, created_at=excluded.created_at")
                .bind(&account_id_text).bind(mailbox).bind(i64::from(uid)).bind(uid_validity).bind(version).bind(&operation_id).bind(Utc::now()).execute(&mut *tx).await?;
            version
        } else {
            0
        };
        let now = Utc::now();
        sqlx::query("INSERT INTO operation_journal(operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', NULL, 0, NULL, NULL, ?, ?)")
            .bind(&operation_id).bind(&account_id_text).bind(target.mailbox).bind(target.uid.map(i64::from)).bind(target.uid_validity.map(|value| i64::try_from(value)).transpose()?).bind(target.message_id).bind(kind).bind(payload_json).bind(local_version).bind(dependency_id).bind(now).bind(now)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        self.operation_journal_entry(&operation_id)
            .await?
            .ok_or_else(|| anyhow!("created operation is missing"))
    }

    /// Atomically stages and leases a direct SMTP submission. This prevents a
    /// concurrent desktop drain from taking the queued row between a CLI
    /// command's durable prepare step and its first SMTP attempt.
    pub async fn enqueue_smtp_submission_and_claim(
        &self,
        account_id: AccountId,
        payload_json: &str,
        claim_owner: &str,
    ) -> Result<OperationJournalEntry> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        serde_json::from_str::<serde_json::Value>(payload_json)
            .context("operation payload must be valid JSON")?;
        let account_id_text = account_id.to_string();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?) AND NOT EXISTS (SELECT 1 FROM account_removal_gates WHERE account_id = ? AND expires_at > ?))")
            .bind(&account_id_text)
            .bind(&account_id_text)
            .bind(&account_id_text)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
        if !account_live {
            tx.rollback().await?;
            return Err(anyhow!("account is unavailable for submission"));
        }
        let active: Option<String> = sqlx::query_scalar("SELECT operation_id FROM operation_journal WHERE account_id = ? AND kind = 'smtp_submission' AND state = 'submitting' ORDER BY claimed_at LIMIT 1")
            .bind(&account_id_text).fetch_optional(&mut *tx).await?;
        let queued_before: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operation_journal WHERE account_id = ? AND kind = 'smtp_submission' AND smtp_accepted_at IS NULL AND state IN ('queued', 'retry'))")
            .bind(&account_id_text)
            .fetch_one(&mut *tx)
            .await?;
        let claimed = active.is_none() && !queued_before;
        sqlx::query("INSERT INTO operation_journal(operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, claim_owner, claimed_at, claimed_from_state, created_at, updated_at) VALUES (?, ?, NULL, NULL, NULL, NULL, 'smtp_submission', ?, 0, NULL, ?, NULL, ?, NULL, NULL, ?, ?, ?, ?, ?)")
            .bind(&operation_id)
            .bind(&account_id_text)
            .bind(payload_json)
            .bind(if claimed { "submitting" } else { "queued" })
            .bind(if claimed { 1 } else { 0 })
            .bind(claimed.then_some(claim_owner))
            .bind(claimed.then_some(now))
            .bind(claimed.then_some("queued"))
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let operation = self
            .operation_journal_entry(&operation_id)
            .await?
            .ok_or_else(|| anyhow!("created SMTP operation is missing"))?;
        if claimed {
            Self::record_first_smtp_claim_wait(&operation);
        }
        Ok(operation)
    }

    /// Resolves a visible message and its currently committed UID namespace
    /// under one SQLite write lease before journaling the operation. Command
    /// handlers must prefer this to constructing a locator from an earlier UI
    /// read, because a UIDVALIDITY replacement can recycle the same number.
    pub async fn enqueue_message_operation_by_id(
        &self,
        account_id: AccountId,
        message_id: &str,
        kind: &str,
        payload_json: &str,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        self.enqueue_message_operation_with_expected_identity(
            account_id,
            message_id,
            None,
            kind,
            payload_json,
            dependency_id,
        )
        .await
    }

    /// Atomically journals a mutation only if the UI receipt still names the
    /// exact committed provider locator. A UIDVALIDITY replacement therefore
    /// fails the command instead of redirecting it to a recycled UID.
    pub async fn enqueue_message_operation_for_identity(
        &self,
        identity: &MessageRemoteIdentity,
        kind: &str,
        payload_json: &str,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        self.enqueue_message_operation_with_expected_identity(
            AccountId::parse_str(&identity.account_id)?,
            &identity.message_id,
            Some(identity),
            kind,
            payload_json,
            dependency_id,
        )
        .await
    }

    async fn enqueue_message_operation_with_expected_identity(
        &self,
        account_id: AccountId,
        message_id: &str,
        expected_identity: Option<&MessageRemoteIdentity>,
        kind: &str,
        payload_json: &str,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        if kind.trim().is_empty() {
            return Err(anyhow!("operation kind is required"));
        }
        serde_json::from_str::<serde_json::Value>(payload_json)
            .context("operation payload must be valid JSON")?;
        let account_id_text = account_id.to_string();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let locator: Option<(String, i64, i64)> = sqlx::query_as("SELECT message.mailbox, message.uid, catalogue.uid_validity FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.id = ? AND message.account_id = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = message.account_id AND removal.expires_at > ?) AND (? IS NULL OR (message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND account.config_generation = ? AND account.config_fingerprint = ?)) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != catalogue.uid_validity)")
            .bind(message_id).bind(&account_id_text).bind(Utc::now()).bind(expected_identity.map(|_| 1_i64)).bind(expected_identity.map(|identity| identity.mailbox.as_str())).bind(expected_identity.map(|identity| identity.uid)).bind(expected_identity.map(|identity| identity.remote_name.as_str())).bind(expected_identity.map(|identity| identity.uid_validity)).bind(expected_identity.map(|identity| identity.account_config_generation)).bind(expected_identity.map(|identity| identity.account_config_fingerprint.as_str())).fetch_optional(&mut *tx).await?;
        let Some((mailbox, uid, uid_validity)) = locator else {
            tx.rollback().await?;
            return Err(anyhow!("message has no committed remote locator"));
        };
        let local_version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) + 1 FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity = ?")
            .bind(&account_id_text).bind(&mailbox).bind(uid).bind(uid_validity).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE folder_sync_state SET local_mutation_version = local_mutation_version + 1, revision = revision + 1, updated_at = ? WHERE account_id = ? AND mailbox = ?")
            .bind(Utc::now()).bind(&account_id_text).bind(&mailbox).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mailbox_mutation_versions(account_id, mailbox, uid, uid_validity, version, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid, uid_validity) DO UPDATE SET version=excluded.version, updated_at=excluded.updated_at")
            .bind(&account_id_text).bind(&mailbox).bind(uid).bind(uid_validity).bind(local_version).bind(Utc::now()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mailbox_mutation_fences(account_id, mailbox, uid, uid_validity, version, operation_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET uid_validity=excluded.uid_validity, version=excluded.version, operation_id=excluded.operation_id, created_at=excluded.created_at")
            .bind(&account_id_text).bind(&mailbox).bind(uid).bind(uid_validity).bind(local_version).bind(&operation_id).bind(Utc::now()).execute(&mut *tx).await?;
        let now = Utc::now();
        sqlx::query("INSERT INTO operation_journal(operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', NULL, 0, NULL, NULL, ?, ?)")
            .bind(&operation_id).bind(&account_id_text).bind(&mailbox).bind(uid).bind(uid_validity).bind(message_id).bind(kind).bind(payload_json).bind(local_version).bind(dependency_id).bind(now).bind(now)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        self.operation_journal_entry(&operation_id)
            .await?
            .ok_or_else(|| anyhow!("created message operation is missing"))
    }

    /// Captures prior flags, writes the durable operation/fence, and applies
    /// the optimistic flag values under one SQLite lease. The journal payload
    /// contains both prior values and immutable remote locator data, so a
    /// worker never builds rollback state from an earlier UI read.
    pub async fn enqueue_and_apply_flag_mutation_for_identity(
        &self,
        identity: &MessageRemoteIdentity,
        kind: &str,
        is_read: Option<bool>,
        is_flagged: Option<bool>,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        if kind.trim().is_empty() || (is_read.is_none() && is_flagged.is_none()) {
            return Err(anyhow!("flag mutation requires kind and a flag value"));
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row: Option<(bool, bool)> = sqlx::query_as("SELECT message.is_read, message.is_flagged FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.id = ? AND message.account_id = ? AND message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND account.config_generation = ? AND account.config_fingerprint = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = message.account_id AND removal.expires_at > ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != catalogue.uid_validity)")
            .bind(&identity.message_id).bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(&identity.remote_name).bind(identity.uid_validity).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(Utc::now())
            .fetch_optional(&mut *tx).await?;
        let Some((previous_read, previous_flagged)) = row else {
            tx.rollback().await?;
            return Err(anyhow!("message remote identity is stale"));
        };
        let version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) + 1 FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity = ?")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).fetch_one(&mut *tx).await?;
        let now = Utc::now();
        sqlx::query("INSERT INTO mailbox_mutation_versions(account_id, mailbox, uid, uid_validity, version, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid, uid_validity) DO UPDATE SET version=excluded.version, updated_at=excluded.updated_at")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(version).bind(now).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mailbox_mutation_fences(account_id, mailbox, uid, uid_validity, version, operation_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET uid_validity=excluded.uid_validity, version=excluded.version, operation_id=excluded.operation_id, created_at=excluded.created_at")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(version).bind(&operation_id).bind(now).execute(&mut *tx).await?;
        let payload_json = serde_json::json!({
            "messageId": &identity.message_id,
            "mailbox": &identity.mailbox,
            "remoteMailbox": &identity.remote_name,
            "uid": identity.uid,
            "uidValidity": identity.uid_validity,
            "previousRead": previous_read,
            "previousFlagged": previous_flagged,
            "isRead": is_read,
            "isFlagged": is_flagged,
        })
        .to_string();
        sqlx::query("INSERT INTO operation_journal(operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', NULL, 0, NULL, NULL, ?, ?)")
            .bind(&operation_id).bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(&identity.message_id).bind(kind).bind(payload_json).bind(version).bind(dependency_id).bind(now).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE messages SET is_read = COALESCE(?, is_read), is_flagged = COALESCE(?, is_flagged) WHERE id = ?")
            .bind(is_read).bind(is_flagged).bind(&identity.message_id).execute(&mut *tx).await?;
        tx.commit().await?;
        self.operation_journal_entry(&operation_id)
            .await?
            .ok_or_else(|| anyhow!("created flag mutation is missing"))
    }

    /// Atomically captures the full source row, journals a mailbox action,
    /// installs its namespace fence, and moves the row into an operation-owned
    /// hidden membership. A restart therefore retains enough state to either
    /// finish the remote-confirmed action or restore the exact visible row.
    pub async fn enqueue_and_apply_mailbox_action_for_identity(
        &self,
        identity: &MessageRemoteIdentity,
        action: crate::mail::MailboxAction,
        dependency_id: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        self.enqueue_and_apply_mailbox_action_inner(identity, action, dependency_id, None)
            .await
    }

    /// Atomically applies the local mailbox-action projection and leases it
    /// to the direct caller, so no background drain can win the interval
    /// between a CLI command's optimistic update and its provider request.
    pub async fn enqueue_and_apply_mailbox_action_and_claim_for_identity(
        &self,
        identity: &MessageRemoteIdentity,
        action: crate::mail::MailboxAction,
        dependency_id: Option<&str>,
        claim_owner: &str,
    ) -> Result<OperationJournalEntry> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        self.enqueue_and_apply_mailbox_action_inner(
            identity,
            action,
            dependency_id,
            Some(claim_owner),
        )
        .await
    }

    async fn enqueue_and_apply_mailbox_action_inner(
        &self,
        identity: &MessageRemoteIdentity,
        action: crate::mail::MailboxAction,
        dependency_id: Option<&str>,
        claim_owner: Option<&str>,
    ) -> Result<OperationJournalEntry> {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        const MESSAGE_SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id = ? AND account_id = ? AND mailbox = ? AND uid = ?";
        let message: Option<MailSummary> = sqlx::query_as(MESSAGE_SQL)
            .bind(&identity.message_id)
            .bind(&identity.account_id)
            .bind(&identity.mailbox)
            .bind(identity.uid)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(message) = message else {
            tx.rollback().await?;
            return Err(anyhow!("message remote identity is stale"));
        };
        let identity_current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state AS catalogue JOIN accounts AS account ON account.id = catalogue.account_id WHERE catalogue.account_id = ? AND catalogue.mailbox = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND account.config_generation = ? AND account.config_fingerprint = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = catalogue.account_id AND removal.expires_at > ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = catalogue.account_id AND replacement.mailbox = catalogue.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(&identity.remote_name).bind(identity.uid_validity).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(Utc::now()).fetch_one(&mut *tx).await?;
        if !identity_current {
            tx.rollback().await?;
            return Err(anyhow!("message remote identity is stale"));
        }
        let version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) + 1 FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity = ?")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).fetch_one(&mut *tx).await?;
        let now = Utc::now();
        sqlx::query("INSERT INTO mailbox_mutation_versions(account_id, mailbox, uid, uid_validity, version, updated_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid, uid_validity) DO UPDATE SET version=excluded.version, updated_at=excluded.updated_at")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(version).bind(now).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mailbox_mutation_fences(account_id, mailbox, uid, uid_validity, version, operation_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET uid_validity=excluded.uid_validity, version=excluded.version, operation_id=excluded.operation_id, created_at=excluded.created_at")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(version).bind(&operation_id).bind(now).execute(&mut *tx).await?;
        let payload_json = serde_json::json!({ "mutation": "mailbox_action", "action": action, "remoteMailbox": &identity.remote_name }).to_string();
        let state = if claim_owner.is_some() {
            "submitting"
        } else {
            "queued"
        };
        sqlx::query("INSERT INTO operation_journal(operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, claim_owner, claimed_at, claimed_from_state, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'mailbox_action', ?, ?, ?, ?, NULL, ?, NULL, NULL, ?, ?, ?, ?, ?)")
            .bind(&operation_id).bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.uid_validity).bind(&identity.message_id).bind(payload_json).bind(version).bind(dependency_id).bind(state).bind(if claim_owner.is_some() { 1 } else { 0 }).bind(claim_owner).bind(claim_owner.map(|_| now)).bind(claim_owner.map(|_| "queued")).bind(now).bind(now).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO operation_message_backups(operation_id, message_id, original_mailbox, original_uid, message_json, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&operation_id).bind(&message.id).bind(&identity.mailbox).bind(identity.uid).bind(serde_json::to_string(&message)?).bind(now).execute(&mut *tx).await?;
        sqlx::query("INSERT OR REPLACE INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, ?, ?, ?)")
            .bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE messages SET mailbox = ? WHERE id = ?")
            .bind(format!("__pending_action__:{operation_id}"))
            .bind(&message.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.operation_journal_entry(&operation_id)
            .await?
            .ok_or_else(|| anyhow!("created mailbox action is missing"))
    }

    /// Restores an optimistic mailbox action from its transactionally captured
    /// backup and terminalizes the matching lease in the same commit.
    pub async fn rollback_and_complete_claimed_mailbox_action(
        &self,
        operation_id: &str,
        claim_owner: &str,
        error: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let backup: Option<(String, String, i64, String, String)> = sqlx::query_as("SELECT backup.message_id, backup.original_mailbox, backup.original_uid, backup.message_json, operation.account_id FROM operation_message_backups AS backup JOIN operation_journal AS operation ON operation.operation_id = backup.operation_id WHERE backup.operation_id = ? AND operation.state = 'submitting' AND operation.claim_owner = ? AND operation.kind = 'mailbox_action'")
            .bind(operation_id).bind(claim_owner).fetch_optional(&mut *tx).await?;
        let Some((message_id, mailbox, uid, message_json, account_id)) = backup else {
            tx.commit().await?;
            return Ok(false);
        };
        let restored = sqlx::query("UPDATE messages SET mailbox = ?, uid = ? WHERE id = ?")
            .bind(&mailbox)
            .bind(uid)
            .bind(&message_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if restored == 0 {
            let message: MailSummary =
                serde_json::from_str(&message_json).context("decode mailbox-action backup")?;
            persist_message(&mut tx, &message).await?;
        }
        sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(&account_id).bind(&mailbox).bind(uid).execute(&mut *tx).await?;
        let completed = sqlx::query("UPDATE operation_journal SET state = 'permanent_failed', outcome = 'local_rollback_completed', error = ?, next_retry_at = NULL, claim_owner = NULL, claimed_at = NULL, claimed_from_state = NULL, updated_at = ? WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ?")
            .bind(error).bind(Utc::now()).bind(operation_id).bind(claim_owner).execute(&mut *tx).await?;
        if completed.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!("operation claim is stale or missing"));
        }
        sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM operation_message_backups WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn operation_journal_entry(
        &self,
        operation_id: &str,
    ) -> Result<Option<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal WHERE operation_id = ?")
            .bind(operation_id).fetch_optional(&self.pool).await?)
    }

    /// Returns independently runnable operations in stable local-version
    /// order. Dependencies are filtered in SQL so a failure in one mailbox
    /// does not block unrelated account work.
    pub async fn pending_operations(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal AS operation WHERE account_id = ? AND operation.state IN ('queued', 'retry', 'sent_copy_pending') AND (operation.next_retry_at IS NULL OR operation.next_retry_at <= ?) AND (operation.dependency_id IS NULL OR EXISTS (SELECT 1 FROM operation_journal AS dependency WHERE dependency.operation_id = operation.dependency_id AND dependency.state = 'completed')) AND NOT EXISTS (SELECT 1 FROM operation_journal AS earlier WHERE earlier.account_id = operation.account_id AND earlier.mailbox = operation.mailbox AND earlier.uid = operation.uid AND earlier.local_version < operation.local_version AND earlier.state NOT IN ('completed', 'rejected', 'permanent_failed', 'uncertain')) ORDER BY COALESCE(operation.mailbox, ''), operation.local_version, operation.created_at")
            .bind(account_id.to_string()).bind(Utc::now()).fetch_all(&self.pool).await?)
    }

    /// Lists accepted SMTP operations that still need a provider Sent-folder
    /// reconciliation. These rows are intentionally absent from the normal
    /// drain because no worker may resend their SMTP transport.
    pub async fn provider_sent_reconciliation_operations(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal WHERE account_id = ? AND state = 'accepted' AND outcome = 'provider_sent_reconciliation' AND (next_retry_at IS NULL OR next_retry_at <= ?) ORDER BY created_at")
            .bind(account_id.to_string()).bind(Utc::now()).fetch_all(&self.pool).await?)
    }

    /// Durable failures and ambiguous outcomes for startup replay and UI
    /// recovery. UIDVALIDITY replacement keeps these visible to callers.
    pub async fn unresolved_operations(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal WHERE account_id = ? AND state IN ('uncertain', 'rejected', 'permanent_failed') ORDER BY updated_at DESC, operation_id DESC")
            .bind(account_id.to_string()).fetch_all(&self.pool).await?)
    }

    /// Returns the next scheduled retry for a message mutation, including a
    /// future retry. Callers can install one bounded wake-up instead of
    /// repeatedly polling the journal.
    pub async fn next_message_mutation_retry_at(
        &self,
        account_id: AccountId,
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar("SELECT MIN(next_retry_at) FROM operation_journal WHERE account_id = ? AND kind IN ('message_read', 'message_star', 'mailbox_action') AND state IN ('queued', 'retry') AND next_retry_at IS NOT NULL")
            .bind(account_id.to_string())
            .fetch_one(&self.pool)
            .await?)
    }

    /// Returns the next delayed Sent-copy or provider-Sent reconciliation for
    /// one account, including future deadlines so startup can arm one timer.
    pub async fn next_sent_reconciliation_retry_at(
        &self,
        account_id: AccountId,
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar("SELECT MIN(next_retry_at) FROM operation_journal WHERE account_id = ? AND kind = 'smtp_submission' AND next_retry_at IS NOT NULL AND ((state = 'accepted' AND outcome = 'provider_sent_reconciliation') OR state = 'sent_copy_pending' OR (state = 'retry' AND outcome = 'sent_copy_retry_scheduled'))")
            .bind(account_id.to_string())
            .fetch_one(&self.pool)
            .await?)
    }

    /// Changes the durable transport outcome after a remote attempt. Ambiguous
    /// SMTP and APPEND outcomes stay fenced and are never silently retried.
    pub async fn update_operation_outcome(
        &self,
        operation_id: &str,
        state: &str,
        outcome: Option<&str>,
        error: Option<&str>,
        next_retry_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        if !matches!(
            state,
            "queued"
                | "retry"
                | "submitting"
                | "accepted"
                | "sent_copy_pending"
                | "completed"
                | "rejected"
                | "permanent_failed"
                | "uncertain"
        ) {
            return Err(anyhow!("invalid operation state"));
        }
        let changed = sqlx::query("UPDATE operation_journal SET state = ?, outcome = ?, error = ?, next_retry_at = ?, attempts = attempts + 1, updated_at = ? WHERE operation_id = ?")
            .bind(state).bind(outcome).bind(error).bind(next_retry_at).bind(Utc::now()).bind(operation_id)
            .execute(&self.pool).await?;
        if changed.rows_affected() != 1 {
            return Err(anyhow!("operation journal entry is missing"));
        }
        Ok(())
    }

    /// Releases a local-mutation fence only after reconciliation has observed
    /// the authoritative provider state or restored the local state after a
    /// permanent failure.
    pub async fn resolve_local_mailbox_mutation(&self, operation_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Restores optimistic flags only while this exact operation still owns
    /// the locator fence and its UIDVALIDITY namespace remains current. A
    /// replacement cannot receive an old operation's rollback.
    pub async fn rollback_message_flags_if_current_operation(
        &self,
        operation_id: &str,
        previous_read: Option<bool>,
        previous_flagged: Option<bool>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let restored = sqlx::query("UPDATE messages SET is_read = COALESCE(?, is_read), is_flagged = COALESCE(?, is_flagged) WHERE id = (SELECT message_id FROM operation_journal WHERE operation_id = ?) AND EXISTS (SELECT 1 FROM operation_journal AS operation JOIN mailbox_mutation_fences AS fence ON fence.operation_id = operation.operation_id JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = operation.account_id AND catalogue.mailbox = operation.mailbox WHERE operation.operation_id = ? AND messages.account_id = operation.account_id AND messages.mailbox = operation.mailbox AND messages.uid = operation.uid AND catalogue.uid_validity = operation.uid_validity)")
            .bind(previous_read).bind(previous_flagged).bind(operation_id).bind(operation_id)
            .execute(&mut *tx).await?.rows_affected();
        if restored == 1 {
            sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
                .bind(operation_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(restored == 1)
    }

    /// Permanently fails a claimed optimistic flag mutation and restores its
    /// prior flags in the same transaction. A crash can therefore never leave
    /// a rolled-back UI paired with a live `submitting` journal fence.
    pub async fn rollback_and_complete_claimed_message_flags(
        &self,
        operation_id: &str,
        claim_owner: &str,
        previous_read: Option<bool>,
        previous_flagged: Option<bool>,
        outcome: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let restored = sqlx::query("UPDATE messages SET is_read = COALESCE(?, is_read), is_flagged = COALESCE(?, is_flagged) WHERE id = (SELECT message_id FROM operation_journal WHERE operation_id = ?) AND EXISTS (SELECT 1 FROM operation_journal AS operation JOIN mailbox_mutation_fences AS fence ON fence.operation_id = operation.operation_id AND fence.uid_validity = operation.uid_validity JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = operation.account_id AND catalogue.mailbox = operation.mailbox WHERE operation.operation_id = ? AND operation.state = 'submitting' AND operation.claim_owner = ? AND messages.account_id = operation.account_id AND messages.mailbox = operation.mailbox AND messages.uid = operation.uid AND catalogue.uid_validity = operation.uid_validity)")
            .bind(previous_read).bind(previous_flagged).bind(operation_id).bind(operation_id).bind(claim_owner)
            .execute(&mut *tx).await?.rows_affected() == 1;
        let completed = sqlx::query("UPDATE operation_journal SET state = 'permanent_failed', outcome = ?, error = ?, next_retry_at = NULL, claim_owner = NULL, claimed_at = NULL, claimed_from_state = NULL, updated_at = ? WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ?")
            .bind(outcome).bind(error).bind(Utc::now()).bind(operation_id).bind(claim_owner)
            .execute(&mut *tx).await?;
        if completed.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!("operation claim is stale or missing"));
        }
        sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(restored)
    }

    /// Stores Gmail's provider-stable identity and the exact observed labels.
    /// Archive filtering must consult this observation rather than treating an
    /// old Inbox label as permanent truth.
    /// Captures a shared Inbox epoch before issuing any Gmail header FETCH.
    /// The receipt remains valid only until any Inbox membership, flags, or
    /// logical label observation is committed for this account.
    pub async fn capture_gmail_inbox_membership_epoch(
        &self,
        account_id: AccountId,
    ) -> Result<GmailInboxMembershipEpochReceipt> {
        let account_id_text = account_id.to_string();
        let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(&account_id_text).bind(&account_id_text).fetch_one(&self.pool).await?;
        if !live {
            return Err(anyhow!("account was removed"));
        }
        let epoch: Option<i64> = sqlx::query_scalar(
            "SELECT epoch FROM gmail_inbox_membership_epochs WHERE account_id = ?",
        )
        .bind(&account_id_text)
        .fetch_optional(&self.pool)
        .await?;
        Ok(GmailInboxMembershipEpochReceipt {
            account_id,
            epoch: epoch.unwrap_or(0),
        })
    }

    /// Atomically applies every Gmail observation from one header FETCH.
    /// The account-wide receipt is captured before the request, when
    /// X-GM-MSGID is not available yet. Any intervening Inbox membership or
    /// local mutation invalidates the whole batch rather than applying stale
    /// labels or deleting a newly restored Inbox locator.
    pub async fn observe_gmail_messages_with_epoch(
        &self,
        receipt: &GmailInboxMembershipEpochReceipt,
        observations: &[GmailMessageObservation],
    ) -> Result<bool> {
        if observations.is_empty() {
            return Ok(true);
        }
        let account_id = receipt.account_id.to_string();
        let mut seen_gmail_ids = std::collections::HashSet::new();
        for observation in observations {
            if observation.gmail_message_id.trim().is_empty() {
                return Err(anyhow!("Gmail message identity is required"));
            }
            if !seen_gmail_ids.insert(observation.gmail_message_id.as_str()) {
                return Err(anyhow!(
                    "a Gmail observation batch cannot contain the same identity twice"
                ));
            }
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let epoch: Option<i64> = sqlx::query_scalar(
            "SELECT epoch FROM gmail_inbox_membership_epochs WHERE account_id = ?",
        )
        .bind(&account_id)
        .fetch_optional(&mut *tx)
        .await?;
        if epoch.unwrap_or(0) != receipt.epoch {
            tx.rollback().await?;
            return Ok(false);
        }

        let now = Utc::now();
        for observation in observations {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ? AND account_id = ?)",
            )
            .bind(&observation.local_message_id)
            .bind(&account_id)
            .fetch_one(&mut *tx)
            .await?;
            if !exists {
                tx.rollback().await?;
                return Ok(false);
            }
            sqlx::query("INSERT INTO gmail_logical_messages(account_id, gmail_message_id, labels_json, observed_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, gmail_message_id) DO UPDATE SET labels_json=excluded.labels_json, observed_at=excluded.observed_at")
                .bind(&account_id).bind(&observation.gmail_message_id).bind(serde_json::to_string(&observation.labels)?).bind(now).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO gmail_message_memberships(account_id, message_id, gmail_message_id) VALUES (?, ?, ?) ON CONFLICT(account_id, message_id) DO UPDATE SET gmail_message_id=excluded.gmail_message_id")
                .bind(&account_id).bind(&observation.local_message_id).bind(&observation.gmail_message_id).execute(&mut *tx).await?;
        }

        for observation in observations {
            if observation
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case("\\Inbox"))
            {
                continue;
            }
            let ids: Vec<String> = sqlx::query_scalar("SELECT message.id FROM gmail_message_memberships AS membership JOIN messages AS message ON message.id = membership.message_id WHERE membership.account_id = ? AND membership.gmail_message_id = ? AND message.mailbox = 'INBOX'")
                .bind(&account_id).bind(&observation.gmail_message_id).fetch_all(&mut *tx).await?;
            for id in &ids {
                for table in [
                    "message_content_fetches",
                    "message_content_cache",
                    "starred_attachment_metadata",
                    "starred_message_bodies",
                    "attachments",
                ] {
                    let statement = format!("DELETE FROM {table} WHERE message_id = ?");
                    sqlx::query(&statement).bind(id).execute(&mut *tx).await?;
                }
                sqlx::query("DELETE FROM messages WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // The membership triggers may already have advanced this epoch. One
        // explicit bump still invalidates an otherwise label-only batch.
        sqlx::query("INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (?, 1, ?) ON CONFLICT(account_id) DO UPDATE SET epoch=epoch+1, updated_at=excluded.updated_at")
            .bind(&account_id).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn observe_gmail_message_with_epoch(
        &self,
        receipt: &GmailInboxMembershipEpochReceipt,
        local_message_id: &str,
        gmail_message_id: &str,
        labels: &[String],
    ) -> Result<bool> {
        self.observe_gmail_messages_with_epoch(
            receipt,
            &[GmailMessageObservation {
                local_message_id: local_message_id.to_owned(),
                gmail_message_id: gmail_message_id.to_owned(),
                labels: labels.to_vec(),
            }],
        )
        .await
    }

    pub async fn observe_gmail_message(
        &self,
        account_id: AccountId,
        local_message_id: &str,
        gmail_message_id: &str,
        labels: &[String],
    ) -> Result<()> {
        let receipt = self
            .capture_gmail_inbox_membership_epoch(account_id)
            .await?;
        if self
            .observe_gmail_message_with_epoch(&receipt, local_message_id, gmail_message_id, labels)
            .await?
        {
            Ok(())
        } else {
            Err(anyhow!("Gmail Inbox membership changed during observation"))
        }
    }

    /// Applies a Gmail label observation to physical Inbox membership. When a
    /// stable Gmail message no longer has `\\Inbox`, only its INBOX locator is
    /// removed; All Mail, Sent, and archive copies remain untouched.
    pub async fn reconcile_gmail_inbox_membership(
        &self,
        account_id: AccountId,
        gmail_message_id: &str,
        labels: &[String],
    ) -> Result<usize> {
        let had_inbox = !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("\\Inbox"));
        let receipt = self
            .capture_gmail_inbox_membership_receipt(account_id, gmail_message_id)
            .await?;
        let before = receipt.memberships.len();
        Ok(usize::from(
            self.reconcile_gmail_inbox_membership_with_receipt(&receipt, labels)
                .await?,
        ) * usize::from(had_inbox)
            * before)
    }

    /// Captures every Inbox physical locator and its mutation version before
    /// an All Mail labels fetch. A later reconciliation rejects the entire
    /// deletion if any membership changed after this capture.
    pub async fn capture_gmail_inbox_membership_receipt(
        &self,
        account_id: AccountId,
        gmail_message_id: &str,
    ) -> Result<GmailInboxMembershipReceipt> {
        let account_id_text = account_id.to_string();
        let observed_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT observed_at FROM gmail_logical_messages WHERE account_id = ? AND gmail_message_id = ?")
            .bind(&account_id_text).bind(gmail_message_id).fetch_optional(&self.pool).await?;
        let memberships = sqlx::query_as("SELECT message.id AS message_id, message.uid, catalogue.uid_validity, COALESCE(version.version, 0) AS version FROM gmail_message_memberships AS membership JOIN messages AS message ON message.id = membership.message_id AND message.account_id = membership.account_id JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox LEFT JOIN mailbox_mutation_versions AS version ON version.account_id = message.account_id AND version.mailbox = message.mailbox AND version.uid = message.uid AND version.uid_validity = catalogue.uid_validity WHERE membership.account_id = ? AND membership.gmail_message_id = ? AND message.mailbox = 'INBOX'")
            .bind(&account_id_text).bind(gmail_message_id).fetch_all(&self.pool).await?;
        Ok(GmailInboxMembershipReceipt {
            account_id,
            gmail_message_id: gmail_message_id.to_owned(),
            observed_at,
            memberships,
        })
    }

    /// Applies a fetched Gmail label observation only if every captured Inbox
    /// membership still has the same UID namespace and local mutation version.
    /// `false` leaves both labels and memberships unchanged for a later retry.
    pub async fn reconcile_gmail_inbox_membership_with_receipt(
        &self,
        receipt: &GmailInboxMembershipReceipt,
        labels: &[String],
    ) -> Result<bool> {
        if receipt.gmail_message_id.trim().is_empty() {
            return Err(anyhow!("Gmail message identity is required"));
        }
        let account_id = receipt.account_id.to_string();
        let gmail_message_id = &receipt.gmail_message_id;
        let labels_json = serde_json::to_string(labels)?;
        let has_inbox = labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("\\Inbox"));
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let labels_current: bool = sqlx::query_scalar("SELECT CASE WHEN ? IS NULL THEN NOT EXISTS (SELECT 1 FROM gmail_logical_messages WHERE account_id = ? AND gmail_message_id = ?) ELSE EXISTS (SELECT 1 FROM gmail_logical_messages WHERE account_id = ? AND gmail_message_id = ? AND observed_at = ?) END")
            .bind(receipt.observed_at).bind(&account_id).bind(gmail_message_id).bind(&account_id).bind(gmail_message_id).bind(receipt.observed_at).fetch_one(&mut *tx).await?;
        if !labels_current {
            tx.rollback().await?;
            return Ok(false);
        }
        for membership in &receipt.memberships {
            let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox WHERE message.id = ? AND message.account_id = ? AND message.mailbox = 'INBOX' AND message.uid = ? AND catalogue.uid_validity = ? AND COALESCE((SELECT version FROM mailbox_mutation_versions WHERE account_id = message.account_id AND mailbox = message.mailbox AND uid = message.uid AND uid_validity = catalogue.uid_validity), 0) = ? AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_fences AS fence WHERE fence.account_id = message.account_id AND fence.mailbox = message.mailbox AND fence.uid = message.uid AND fence.uid_validity = catalogue.uid_validity))")
                .bind(&membership.message_id).bind(&account_id).bind(membership.uid).bind(membership.uid_validity).bind(membership.version).fetch_one(&mut *tx).await?;
            if !current {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        sqlx::query("INSERT INTO gmail_logical_messages(account_id, gmail_message_id, labels_json, observed_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, gmail_message_id) DO UPDATE SET labels_json=excluded.labels_json, observed_at=excluded.observed_at")
            .bind(&account_id).bind(gmail_message_id).bind(labels_json).bind(Utc::now()).execute(&mut *tx).await?;
        if has_inbox {
            tx.commit().await?;
            return Ok(true);
        }
        let ids: Vec<String> = receipt
            .memberships
            .iter()
            .map(|membership| membership.message_id.clone())
            .collect();
        for id in &ids {
            for table in [
                "message_content_fetches",
                "message_content_cache",
                "starred_attachment_metadata",
                "starred_message_bodies",
                "attachments",
            ] {
                let statement = format!("DELETE FROM {table} WHERE message_id = ?");
                sqlx::query(&statement).bind(id).execute(&mut *tx).await?;
            }
        }
        let deleted = if ids.is_empty() {
            0
        } else {
            let placeholders = vec!["?"; ids.len()].join(",");
            let statement = format!("DELETE FROM messages WHERE id IN ({placeholders})");
            let mut query = sqlx::query(&statement);
            for id in &ids {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await?.rows_affected()
        };
        tx.commit().await?;
        Ok(deleted == u64::try_from(ids.len())?)
    }

    /// Returns the durable rotating UID cursor for bounded Gmail label
    /// reconciliation. `None` means begin a fresh pass from the newest
    /// eligible locator.
    pub async fn gmail_label_reconciliation_cursor(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<Option<u32>> {
        let cursor: Option<i64> = sqlx::query_scalar(
            "SELECT cursor_uid FROM gmail_label_reconciliation_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        cursor
            .map(|value| u32::try_from(value).context("stored Gmail label cursor is invalid"))
            .transpose()
    }

    /// Returns both the durable cursor and the bounded range span. A missing
    /// row starts at 64 UIDs; protocol may grow only after a tagged-OK empty
    /// range and reset after any observed result.
    pub async fn gmail_label_reconciliation_cursor_with_span(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<GmailLabelReconciliationCursor> {
        let row: Option<(Option<i64>, i64)> = sqlx::query_as(
            "SELECT cursor_uid, span FROM gmail_label_reconciliation_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await?;
        let (cursor_uid, span) = row.unwrap_or((None, 64));
        Ok(GmailLabelReconciliationCursor {
            cursor_uid: cursor_uid
                .map(|value| u32::try_from(value).context("stored Gmail label cursor is invalid"))
                .transpose()?,
            span: u32::try_from(span).context("stored Gmail label span is invalid")?,
        })
    }

    /// Advances the durable Gmail label reconciliation cursor after a fully
    /// observed bounded page. Passing `None` records the completed pass and
    /// makes the next scheduled pass begin from the newest locator again.
    pub async fn advance_gmail_label_reconciliation_cursor(
        &self,
        account_id: AccountId,
        mailbox: &str,
        next: Option<u32>,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(&account_id)
            .bind(&account_id)
            .fetch_one(&mut *tx)
            .await?;
        if !account_live {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        sqlx::query("INSERT INTO gmail_label_reconciliation_state(account_id, mailbox, cursor_uid, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET cursor_uid=excluded.cursor_uid, updated_at=excluded.updated_at")
            .bind(&account_id)
            .bind(mailbox)
            .bind(next.map(i64::from))
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically persists the next sparse-safe cursor and range span after a
    /// complete Gmail label observation page.
    pub async fn advance_gmail_label_reconciliation_cursor_with_span(
        &self,
        account_id: AccountId,
        mailbox: &str,
        next: Option<u32>,
        span: u32,
    ) -> Result<()> {
        if !(1..=65_536).contains(&span) {
            return Err(anyhow!("Gmail label reconciliation span is invalid"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(&account_id)
            .bind(&account_id)
            .fetch_one(&mut *tx)
            .await?;
        if !account_live {
            tx.rollback().await?;
            return Err(anyhow!("account was removed"));
        }
        sqlx::query("INSERT INTO gmail_label_reconciliation_state(account_id, mailbox, cursor_uid, span, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET cursor_uid=excluded.cursor_uid, span=excluded.span, updated_at=excluded.updated_at")
            .bind(&account_id)
            .bind(mailbox)
            .bind(next.map(i64::from))
            .bind(i64::from(span))
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn gmail_message_labels(
        &self,
        account_id: AccountId,
        local_message_id: &str,
    ) -> Result<Option<Vec<String>>> {
        let labels: Option<String> = sqlx::query_scalar("SELECT logical.labels_json FROM gmail_message_memberships AS membership JOIN gmail_logical_messages AS logical ON logical.account_id = membership.account_id AND logical.gmail_message_id = membership.gmail_message_id WHERE membership.account_id = ? AND membership.message_id = ?")
            .bind(account_id.to_string()).bind(local_message_id).fetch_optional(&self.pool).await?;
        labels
            .map(|value| serde_json::from_str(&value).context("stored Gmail labels are invalid"))
            .transpose()
    }

    /// Retains the provider INTERNALDATE separately from the parsed RFC 5322
    /// Date. The former is stable mailbox ordering evidence; the latter stays
    /// available for display without forcing legacy catalogue migrations.
    pub async fn observe_message_dates(
        &self,
        account_id: AccountId,
        local_message_id: &str,
        internal_date: DateTime<Utc>,
        message_date: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ? AND account_id = ?)",
        )
        .bind(local_message_id)
        .bind(&account_id)
        .fetch_one(&self.pool)
        .await?;
        if !exists {
            return Err(anyhow!(
                "message date observation does not match the account message"
            ));
        }
        sqlx::query("INSERT INTO message_temporal_observations(account_id, message_id, internal_date, message_date, observed_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(account_id, message_id) DO UPDATE SET internal_date=excluded.internal_date, message_date=excluded.message_date, observed_at=excluded.observed_at")
            .bind(account_id).bind(local_message_id).bind(internal_date).bind(message_date).bind(Utc::now())
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn delete_account(&self, id: AccountId) -> Result<()> {
        self.delete_account_inner(id, None, None).await
    }

    /// Deletes an account and its account-owned encrypted credential in the
    /// same SQLite transaction. If either deletion cannot be recorded, both
    /// the account and secret remain available for recovery.
    pub async fn delete_account_and_secret(&self, id: AccountId, secret_name: &str) -> Result<()> {
        if secret_name.trim().is_empty() {
            return Err(anyhow!("account credential name is required"));
        }
        self.delete_account_inner(id, Some(secret_name), None).await
    }

    pub async fn delete_account_and_secret_with_removal_gate(
        &self,
        id: AccountId,
        secret_name: &str,
        owner: &str,
    ) -> Result<()> {
        if secret_name.trim().is_empty() || owner.trim().is_empty() {
            return Err(anyhow!(
                "account credential name and removal owner are required"
            ));
        }
        self.delete_account_inner(id, Some(secret_name), Some(owner))
            .await
    }

    async fn delete_account_inner(
        &self,
        id: AccountId,
        secret_name: Option<&str>,
        expected_owner: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now();
        sqlx::query("INSERT INTO account_removal_gates(account_id, owner, blocked_at, expires_at) SELECT ?, 'delete', ?, ? WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?) ON CONFLICT(account_id) DO NOTHING")
            .bind(id.to_string())
            .bind(now)
            .bind(now + chrono::Duration::seconds(ACCOUNT_REMOVAL_GATE_LEASE_SECONDS))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        if let Some(owner) = expected_owner {
            let owns_gate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_removal_gates WHERE account_id = ? AND owner = ? AND expires_at > ?)")
                .bind(id.to_string()).bind(owner).bind(now).fetch_one(&mut *tx).await?;
            if !owns_gate {
                tx.rollback().await?;
                return Err(anyhow!("account removal lease was lost"));
            }
        }
        sqlx::query("INSERT INTO deleted_account_tombstones(account_id, deleted_at) VALUES (?, ?) ON CONFLICT(account_id) DO UPDATE SET deleted_at=excluded.deleted_at")
            .bind(id.to_string())
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mail_rebuild_jobs WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        for table in [
            "sync_runs",
            "folder_sync_state",
            "mailbox_mutation_fences",
            "operation_journal",
            "gmail_message_memberships",
            "gmail_logical_messages",
            "gmail_label_reconciliation_state",
            "message_temporal_observations",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account_id = ?"))
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
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
        sqlx::query("DELETE FROM messages WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        if let Some(secret_name) = secret_name {
            sqlx::query("DELETE FROM credentials WHERE name = ?")
                .bind(secret_name)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically blocks every durable operation claim for an account before
    /// a caller waits for any in-flight SMTP attempt. The row is shared by
    /// desktop and CLI processes using the same database, so a second process
    /// cannot claim a queued submission while removal is in progress.
    pub async fn begin_account_removal(&self, id: AccountId) -> Result<bool> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(&account_id)
            .bind(&account_id)
            .fetch_one(&mut *tx)
            .await?;
        if !account_live {
            tx.commit().await?;
            return Ok(false);
        }
        let now = Utc::now();
        sqlx::query("INSERT INTO account_removal_gates(account_id, owner, blocked_at, expires_at) VALUES (?, 'legacy-account-removal', ?, ?) ON CONFLICT(account_id) DO UPDATE SET owner=excluded.owner, blocked_at=excluded.blocked_at, expires_at=excluded.expires_at")
            .bind(&account_id)
            .bind(now)
            .bind(now + chrono::Duration::seconds(ACCOUNT_REMOVAL_GATE_LEASE_SECONDS))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Acquires an owner-scoped removal gate only when no other live owner
    /// holds it. New lifecycle code should use this instead of the legacy
    /// account-only helper.
    pub async fn begin_account_removal_with_owner(
        &self,
        id: AccountId,
        owner: &str,
    ) -> Result<bool> {
        if owner.trim().is_empty() {
            return Err(anyhow!("account removal owner is required"));
        }
        let account_id = id.to_string();
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let claimed = sqlx::query("INSERT INTO account_removal_gates(account_id, owner, blocked_at, expires_at) SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)) ON CONFLICT(account_id) DO UPDATE SET owner=excluded.owner, blocked_at=excluded.blocked_at, expires_at=excluded.expires_at WHERE account_removal_gates.expires_at <= ? OR account_removal_gates.owner = excluded.owner")
            .bind(&account_id).bind(owner).bind(now).bind(now + chrono::Duration::seconds(ACCOUNT_REMOVAL_GATE_LEASE_SECONDS)).bind(&account_id).bind(&account_id).bind(now)
            .execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(claimed == 1)
    }

    pub async fn renew_account_removal_gate(&self, id: AccountId, owner: &str) -> Result<bool> {
        Ok(sqlx::query("UPDATE account_removal_gates SET expires_at = ? WHERE account_id = ? AND owner = ? AND expires_at > ?")
            .bind(Utc::now() + chrono::Duration::seconds(ACCOUNT_REMOVAL_GATE_LEASE_SECONDS))
            .bind(id.to_string()).bind(owner).bind(Utc::now())
            .execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn release_account_removal_gate(&self, id: AccountId, owner: &str) -> Result<bool> {
        Ok(
            sqlx::query("DELETE FROM account_removal_gates WHERE account_id = ? AND owner = ?")
                .bind(id.to_string())
                .bind(owner)
                .execute(&self.pool)
                .await?
                .rows_affected()
                == 1,
        )
    }

    pub async fn acquire_account_removal_gate(
        &self,
        id: AccountId,
    ) -> Result<Option<AccountOperationGate>> {
        let owner = uuid::Uuid::new_v4().to_string();
        if !self.begin_account_removal_with_owner(id, &owner).await? {
            return Ok(None);
        }
        let store = self.clone();
        let heartbeat_owner = owner.clone();
        let task = tokio::spawn(async move {
            let interval = Duration::from_secs(
                u64::try_from(ACCOUNT_REMOVAL_GATE_LEASE_SECONDS / 3).unwrap_or(40),
            );
            loop {
                tokio::time::sleep(interval).await;
                match store.renew_account_removal_gate(id, &heartbeat_owner).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        });
        Ok(Some(AccountOperationGate {
            store: self.clone(),
            account_id: id,
            owner,
            task,
        }))
    }

    /// Reopens the durable operation queue when account removal is abandoned.
    /// A deleted account keeps its irreversible tombstone and cannot be
    /// reopened through this API.
    pub async fn cancel_account_removal(&self, id: AccountId) -> Result<bool> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let released = sqlx::query("DELETE FROM account_removal_gates WHERE account_id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)")
            .bind(&account_id)
            .bind(&account_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(released == 1)
    }

    /// Clears only expired removal leases for still-live accounts. It is safe
    /// to call during startup from either desktop or CLI because an active
    /// removal has a future expiry and remains blocked.
    pub async fn recover_orphan_account_operation_gates(&self) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM account_removal_gates WHERE expires_at <= ? AND EXISTS (SELECT 1 FROM accounts WHERE accounts.id = account_removal_gates.account_id) AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = account_removal_gates.account_id)")
            .bind(Utc::now())
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Deletes only provider-derived local mail state for an account. Account
    /// configuration and encrypted credentials remain intact so a subsequent
    /// full catalogue sync can rebuild from the authoritative provider.
    pub async fn reset_account_mail_index(&self, id: AccountId) -> Result<()> {
        let account_id = id.to_string();
        let mut tx = self.pool.begin().await?;
        for statement in [
            "DELETE FROM starred_attachment_metadata WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM starred_message_bodies WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?)",
            "DELETE FROM mailbox_catalog_state WHERE account_id = ?",
            "DELETE FROM mailbox_snapshot_generations WHERE account_id = ?",
            "DELETE FROM mailbox_sync_failures WHERE account_id = ?",
            "DELETE FROM mailbox_sync_state WHERE account_id = ?",
            "DELETE FROM mailbox_action_tombstones WHERE account_id = ?",
            "DELETE FROM sync_runs WHERE account_id = ?",
            "DELETE FROM folder_sync_state WHERE account_id = ?",
            "DELETE FROM mailbox_mutation_fences WHERE account_id = ?",
            "DELETE FROM operation_journal WHERE account_id = ?",
            "DELETE FROM gmail_message_memberships WHERE account_id = ?",
            "DELETE FROM gmail_logical_messages WHERE account_id = ?",
            "DELETE FROM gmail_label_reconciliation_state WHERE account_id = ?",
            "DELETE FROM message_temporal_observations WHERE account_id = ?",
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

    /// Commits provider metadata only while the mailbox is still the exact
    /// namespace selected before FETCH. Realtime and bounded catalogue
    /// workers must use this instead of an unfenced catalogue upsert.
    ///
    /// `false` means a UIDVALIDITY change, remote-name remap, or replacement
    /// generation won the race. No message from the stale response was
    /// written.
    pub async fn commit_provider_messages_if_current(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        messages: &[MailSummary],
    ) -> Result<bool> {
        let account_id = account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_id || message.mailbox != mailbox)
        {
            return Err(anyhow!(
                "provider batch message does not match the requested account or mailbox"
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state AS catalogue JOIN accounts AS account ON account.id = catalogue.account_id WHERE catalogue.account_id = ? AND catalogue.mailbox = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = catalogue.account_id AND replacement.mailbox = catalogue.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&account_id).bind(mailbox).bind(remote_name).bind(i64::from(uid_validity))
            .fetch_one(&mut *tx).await?;
        if !current {
            tx.rollback().await?;
            return Ok(false);
        }
        for message in messages {
            let mut message = message.clone();
            if let Some(canonical_id) = sqlx::query_scalar::<_, String>(
                "SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
            )
            .bind(&account_id)
            .bind(mailbox)
            .bind(message.uid)
            .fetch_optional(&mut *tx)
            .await?
            {
                message.id = canonical_id;
            } else {
                message.id = uidvalidity_message_id(
                    AccountId::parse_str(&account_id)?,
                    mailbox,
                    u32::try_from(message.uid).context("provider message UID is invalid")?,
                    uid_validity,
                );
            }
            for attachment in &mut message.attachments {
                attachment.attachment.message_id = message.id.clone();
            }
            persist_message_with_flag_policy(
                &mut tx,
                &message,
                FlagUpdatePolicy::ProviderAuthoritative,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Establishes the immutable catalogue identity for a legacy profile
    /// before its first fenced provider write. It only creates an absent row;
    /// a different existing namespace or any pending replacement is rejected.
    pub async fn ensure_mailbox_catalog_identity(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
    ) -> Result<bool> {
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let replacement_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND uid_validity != ?)")
            .bind(&account_id).bind(mailbox).bind(i64::from(uid_validity)).fetch_one(&mut *tx).await?;
        if replacement_pending {
            tx.rollback().await?;
            return Ok(false);
        }
        let existing: Option<(String, i64, Option<i64>, Option<String>)> = sqlx::query_as("SELECT remote_name, uid_validity, provider_config_generation, provider_config_fingerprint FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?")
            .bind(&account_id).bind(mailbox).fetch_optional(&mut *tx).await?;
        if let Some((existing_remote, existing_uid_validity, generation, fingerprint)) = existing {
            let current: Option<(i64, String)> = sqlx::query_as(
                "SELECT config_generation, config_fingerprint FROM accounts WHERE id = ?",
            )
            .bind(&account_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(
                current.is_some_and(|(current_generation, current_fingerprint)| {
                    existing_remote == remote_name
                        && existing_uid_validity == i64::from(uid_validity)
                        && generation == Some(current_generation)
                        && fingerprint.as_deref() == Some(current_fingerprint.as_str())
                }),
            );
        }
        let account_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(&account_id).bind(&account_id).fetch_one(&mut *tx).await?;
        if !account_exists {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, provider_config_generation, provider_config_fingerprint, updated_at) SELECT ?, ?, ?, ?, 0, 0, NULL, NULL, config_generation, config_fingerprint, ? FROM accounts WHERE id = ?")
            .bind(&account_id).bind(mailbox).bind(remote_name).bind(i64::from(uid_validity)).bind(Utc::now()).bind(&account_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Establishes a missing catalogue namespace from the exact Account that
    /// opened the IMAP connection. Existing rows whose provider stamp was
    /// invalidated by an account update are deliberately not revived here:
    /// they need an authenticated header/snapshot publication first.
    pub async fn ensure_mailbox_catalog_identity_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
    ) -> Result<bool> {
        let Some(receipt) = self.capture_account_provider_receipt(account).await? else {
            return Ok(false);
        };
        let account_id = account.id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if ensure_account_provider_receipt_current_in_transaction(&mut tx, &receipt)
            .await
            .is_err()
        {
            tx.rollback().await?;
            return Ok(false);
        }
        let existing: Option<(String, i64, Option<i64>, Option<String>)> = sqlx::query_as("SELECT remote_name, uid_validity, provider_config_generation, provider_config_fingerprint FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?")
            .bind(&account_id)
            .bind(mailbox)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((existing_remote, existing_uid_validity, generation, fingerprint)) = existing {
            tx.commit().await?;
            return Ok(existing_remote == remote_name
                && existing_uid_validity == i64::from(uid_validity)
                && generation == Some(receipt.config_generation)
                && fingerprint.as_deref() == Some(receipt.config_fingerprint.as_str()));
        }
        let replacement_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND uid_validity != ?)")
            .bind(&account_id).bind(mailbox).bind(i64::from(uid_validity)).fetch_one(&mut *tx).await?;
        if replacement_pending {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, provider_config_generation, provider_config_fingerprint, updated_at) VALUES (?, ?, ?, ?, 0, 0, NULL, NULL, ?, ?, ?)")
            .bind(&account_id).bind(mailbox).bind(remote_name).bind(i64::from(uid_validity)).bind(receipt.config_generation).bind(&receipt.config_fingerprint).bind(Utc::now()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Captures the namespace and per-UID local flags before opening IMAP.
    /// Pass the returned receipt to [`Self::commit_provider_messages_with_receipt`]
    /// after FETCH so a completed optimistic mutation still wins over an old
    /// provider response.
    /// Compatibility capture for code that has not retained the Account used
    /// for the connection. Provider callers must use
    /// [`Self::capture_provider_write_receipt_for_account`] before IMAP I/O.
    pub async fn capture_provider_write_receipt(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        uids: &[u32],
    ) -> Result<Option<ProviderWriteReceipt>> {
        let Some(account) = self.account(account_id).await? else {
            return Ok(None);
        };
        self.capture_provider_write_receipt_for_account(
            &account,
            mailbox,
            remote_name,
            uid_validity,
            uids,
        )
        .await
    }

    /// Captures a provider write fence from the exact Account used to open the
    /// IMAP connection. An account endpoint/auth change after this point makes
    /// the later commit fail, even if the new provider has coincidentally equal
    /// mailbox names and UIDVALIDITY values.
    pub async fn capture_provider_write_receipt_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        uids: &[u32],
    ) -> Result<Option<ProviderWriteReceipt>> {
        let account_id = account.id;
        let account_id_text = account_id.to_string();
        let Some(account_receipt) = self.capture_account_provider_receipt(account).await? else {
            return Ok(None);
        };
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = ? AND catalogue.mailbox = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = ? AND catalogue.provider_config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = catalogue.account_id AND replacement.mailbox = catalogue.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&account_id_text).bind(mailbox).bind(remote_name).bind(i64::from(uid_validity)).bind(account_receipt.config_generation).bind(&account_receipt.config_fingerprint)
            .fetch_one(&self.pool).await?;
        if !current {
            return Ok(None);
        }
        let expected_flags = self
            .capture_recent_catalogue_expected_flags(account_id, mailbox, uids)
            .await?;
        let local_mutation_versions = if uids.is_empty() {
            Vec::new()
        } else {
            let placeholders = vec!["?"; uids.len()].join(",");
            let sql = format!(
                "SELECT uid, version FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid_validity = ? AND uid IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, ProviderMessageVersion>(&sql)
                .bind(&account_id_text)
                .bind(mailbox)
                .bind(i64::from(uid_validity));
            for uid in uids {
                query = query.bind(i64::from(*uid));
            }
            query.fetch_all(&self.pool).await?
        };
        Ok(Some(ProviderWriteReceipt {
            account_id,
            account_config_generation: account_receipt.config_generation,
            account_config_fingerprint: account_receipt.config_fingerprint,
            mailbox: mailbox.to_owned(),
            remote_name: remote_name.to_owned(),
            uid_validity,
            expected_flags,
            local_mutation_versions,
        }))
    }

    /// Captures the exact persisted transport configuration of an already
    /// connected provider Account. Folder/snapshot writers which do not carry
    /// a per-page provider receipt must retain this before their first IMAP
    /// command and reject publication when it is no longer current.
    pub async fn capture_account_provider_receipt(
        &self,
        account: &Account,
    ) -> Result<Option<AccountProviderReceipt>> {
        let account_id = account.id.to_string();
        let config_fingerprint = account_provider_config_fingerprint(account)?;
        let config_generation: Option<i64> = sqlx::query_scalar(
            "SELECT config_generation FROM accounts WHERE id = ? AND config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(&account_id)
        .bind(&config_fingerprint)
        .bind(&account_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            config_generation.map(|config_generation| AccountProviderReceipt {
                account_id: account.id,
                config_generation,
                config_fingerprint,
            }),
        )
    }

    /// Tests an account receipt inside the caller's provider publication
    /// boundary. `false` requires discarding that old connection's response.
    pub async fn account_provider_receipt_is_current(
        &self,
        receipt: &AccountProviderReceipt,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND config_generation = ? AND config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
            .bind(receipt.account_id.to_string())
            .bind(receipt.config_generation)
            .bind(&receipt.config_fingerprint)
            .bind(receipt.account_id.to_string())
            .fetch_one(&self.pool).await?)
    }

    /// Fenced provider publication for a receipt captured before FETCH. The
    /// operation is all-or-nothing: a stale namespace produces `false`, while
    /// a local mutation after capture preserves its newer flags.
    /// Fenced provider publication for a receipt captured before FETCH. The
    /// operation is all-or-nothing: a stale namespace produces `false`, while
    /// a local mutation after capture preserves its newer flags.
    pub async fn commit_provider_messages_with_receipt(
        &self,
        receipt: &ProviderWriteReceipt,
        messages: &[MailSummary],
    ) -> Result<bool> {
        self.commit_provider_messages_with_receipt_inner(receipt, messages, None)
            .await
    }

    /// Publishes parsed provider headers and their Gmail stable identities in
    /// one immediate transaction. The Gmail epoch is checked before the
    /// header rows are written, so this batch's own Inbox inserts cannot make
    /// a valid pre-FETCH observation look stale.
    pub async fn commit_provider_messages_with_receipt_and_gmail_observations(
        &self,
        receipt: &ProviderWriteReceipt,
        messages: &[MailSummary],
        gmail_receipt: &GmailInboxMembershipEpochReceipt,
        observations: &[GmailProviderObservation],
    ) -> Result<bool> {
        if gmail_receipt.account_id != receipt.account_id {
            return Err(anyhow!("Gmail epoch receipt belongs to another account"));
        }
        self.commit_provider_messages_with_receipt_inner(
            receipt,
            messages,
            Some((gmail_receipt, observations)),
        )
        .await
    }

    async fn commit_provider_messages_with_receipt_inner(
        &self,
        receipt: &ProviderWriteReceipt,
        messages: &[MailSummary],
        gmail_observations: Option<(
            &GmailInboxMembershipEpochReceipt,
            &[GmailProviderObservation],
        )>,
    ) -> Result<bool> {
        let account_id = receipt.account_id.to_string();
        if messages
            .iter()
            .any(|message| message.account_id != account_id || message.mailbox != receipt.mailbox)
        {
            return Err(anyhow!("provider batch does not match its write receipt"));
        }
        let expected_by_locator: HashMap<(&str, &str, i64), (bool, bool)> = receipt
            .expected_flags
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
        let expected_versions: HashMap<i64, i64> = receipt
            .local_mutation_versions
            .iter()
            .map(|version| (version.uid, version.version))
            .collect();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = ? AND catalogue.mailbox = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = ? AND catalogue.provider_config_fingerprint = ? AND EXISTS (SELECT 1 FROM accounts AS account WHERE account.id = catalogue.account_id AND account.config_generation = ? AND account.config_fingerprint = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = catalogue.account_id AND replacement.mailbox = catalogue.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&account_id).bind(&receipt.mailbox).bind(&receipt.remote_name).bind(i64::from(receipt.uid_validity)).bind(receipt.account_config_generation).bind(&receipt.account_config_fingerprint).bind(receipt.account_config_generation).bind(&receipt.account_config_fingerprint)
            .fetch_one(&mut *tx).await?;
        if !current {
            tx.rollback().await?;
            return Ok(false);
        }
        if let Some((gmail_receipt, observations)) = gmail_observations {
            let current_epoch: Option<i64> = sqlx::query_scalar(
                "SELECT epoch FROM gmail_inbox_membership_epochs WHERE account_id = ?",
            )
            .bind(&account_id)
            .fetch_optional(&mut *tx)
            .await?;
            if current_epoch.unwrap_or(0) != gmail_receipt.epoch
                || observations.iter().any(|observation| {
                    observation.uid == 0 || observation.gmail_message_id.trim().is_empty()
                })
            {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        for message in messages {
            let mut message = message.clone();
            if let Some(canonical_id) = sqlx::query_scalar::<_, String>(
                "SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
            )
            .bind(&account_id)
            .bind(&receipt.mailbox)
            .bind(message.uid)
            .fetch_optional(&mut *tx)
            .await?
            {
                message.id = canonical_id;
            } else {
                message.id = uidvalidity_message_id(
                    receipt.account_id,
                    &receipt.mailbox,
                    u32::try_from(message.uid).context("provider message UID is invalid")?,
                    receipt.uid_validity,
                );
            }
            for attachment in &mut message.attachments {
                attachment.attachment.message_id = message.id.clone();
            }
            let current_version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity = ?")
                .bind(&account_id).bind(&receipt.mailbox).bind(message.uid).bind(i64::from(receipt.uid_validity)).fetch_one(&mut *tx).await?;
            let version_matches =
                current_version == expected_versions.get(&message.uid).copied().unwrap_or(0);
            persist_message_with_flag_policy(
                &mut tx,
                &message,
                if version_matches {
                    FlagUpdatePolicy::CompareAndSwap(
                        expected_by_locator
                            .get(&(
                                message.account_id.as_str(),
                                message.mailbox.as_str(),
                                message.uid,
                            ))
                            .copied(),
                    )
                } else {
                    FlagUpdatePolicy::PreserveLocal
                },
            )
            .await?;
        }
        if let Some((_gmail_receipt, observations)) = gmail_observations {
            let now = Utc::now();
            for observation in observations {
                let local_message_id: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?",
                )
                .bind(&account_id)
                .bind(&receipt.mailbox)
                .bind(i64::from(observation.uid))
                .fetch_optional(&mut *tx)
                .await?;
                let Some(local_message_id) = local_message_id else {
                    tx.rollback().await?;
                    return Ok(false);
                };
                sqlx::query("INSERT INTO gmail_logical_messages(account_id, gmail_message_id, labels_json, observed_at) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, gmail_message_id) DO UPDATE SET labels_json=excluded.labels_json, observed_at=excluded.observed_at")
                    .bind(&account_id).bind(&observation.gmail_message_id).bind(serde_json::to_string(&observation.labels)?).bind(now).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO gmail_message_memberships(account_id, message_id, gmail_message_id) VALUES (?, ?, ?) ON CONFLICT(account_id, message_id) DO UPDATE SET gmail_message_id=excluded.gmail_message_id")
                    .bind(&account_id).bind(&local_message_id).bind(&observation.gmail_message_id).execute(&mut *tx).await?;
            }
            for observation in observations {
                if observation
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("\\Inbox"))
                {
                    continue;
                }
                let ids: Vec<String> = sqlx::query_scalar("SELECT message.id FROM gmail_message_memberships AS membership JOIN messages AS message ON message.id = membership.message_id WHERE membership.account_id = ? AND membership.gmail_message_id = ? AND message.mailbox = 'INBOX'")
                    .bind(&account_id)
                    .bind(&observation.gmail_message_id)
                    .fetch_all(&mut *tx)
                    .await?;
                for id in ids {
                    sqlx::query("DELETE FROM messages WHERE id = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            sqlx::query("INSERT INTO gmail_inbox_membership_epochs(account_id, epoch, updated_at) VALUES (?, 1, ?) ON CONFLICT(account_id) DO UPDATE SET epoch=epoch+1, updated_at=excluded.updated_at")
                .bind(&account_id).bind(now).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Captures per-UID local mutation versions before a CONDSTORE command.
    pub async fn capture_changed_since_write_receipt(
        &self,
        account_id: AccountId,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        uids: &[u32],
    ) -> Result<Option<ChangedSinceWriteReceipt>> {
        let receipt = self
            .capture_provider_write_receipt(account_id, mailbox, remote_name, uid_validity, uids)
            .await?;
        Ok(receipt.map(|receipt| ChangedSinceWriteReceipt {
            account_id: receipt.account_id,
            account_config_generation: receipt.account_config_generation,
            account_config_fingerprint: receipt.account_config_fingerprint,
            mailbox: receipt.mailbox,
            remote_name: receipt.remote_name,
            uid_validity: receipt.uid_validity,
            local_mutation_versions: receipt.local_mutation_versions,
        }))
    }

    /// Account-bound CONDSTORE receipt. Use this with the Account that opened
    /// the IMAP connection, before issuing the delta command.
    pub async fn capture_changed_since_write_receipt_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        remote_name: &str,
        uid_validity: u32,
        uids: &[u32],
    ) -> Result<Option<ChangedSinceWriteReceipt>> {
        let receipt = self
            .capture_provider_write_receipt_for_account(
                account,
                mailbox,
                remote_name,
                uid_validity,
                uids,
            )
            .await?;
        Ok(receipt.map(|receipt| ChangedSinceWriteReceipt {
            account_id: receipt.account_id,
            account_config_generation: receipt.account_config_generation,
            account_config_fingerprint: receipt.account_config_fingerprint,
            mailbox: receipt.mailbox,
            remote_name: receipt.remote_name,
            uid_validity: receipt.uid_validity,
            local_mutation_versions: receipt.local_mutation_versions,
        }))
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
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, provider_config_generation, provider_config_fingerprint, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, (SELECT config_generation FROM accounts WHERE id = ?), (SELECT config_fingerprint FROM accounts WHERE id = ?), ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, remote_total=excluded.remote_total, historical_complete=excluded.historical_complete, provider_config_generation=excluded.provider_config_generation, provider_config_fingerprint=excluded.provider_config_fingerprint, updated_at=excluded.updated_at")
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(remote_name)
            .bind(uid_validity)
            .bind(remote_total as i64)
            .bind(historical_complete)
            .bind(account_id.to_string())
            .bind(account_id.to_string())
            .bind(Utc::now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Updates catalogue progress only if the namespace captured before a
    /// provider fetch is still the committed namespace. This is intentionally
    /// separate from provider-header publication because callers may learn
    /// EXISTS or completion metadata after their last bounded batch.
    pub async fn save_mailbox_catalog_state_if_current(
        &self,
        receipt: &ProviderWriteReceipt,
        remote_total: usize,
        historical_complete: bool,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE mailbox_catalog_state SET remote_total = ?, historical_complete = ?, updated_at = ? WHERE account_id = ? AND mailbox = ? AND remote_name = ? AND uid_validity = ? AND provider_config_generation = ? AND provider_config_fingerprint = ? AND EXISTS (SELECT 1 FROM accounts AS account WHERE account.id = mailbox_catalog_state.account_id AND account.config_generation = ? AND account.config_fingerprint = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = mailbox_catalog_state.account_id AND replacement.mailbox = mailbox_catalog_state.mailbox AND replacement.uid_validity != mailbox_catalog_state.uid_validity)")
            .bind(i64::try_from(remote_total).context("remote mailbox total is invalid")?)
            .bind(historical_complete)
            .bind(Utc::now())
            .bind(receipt.account_id.to_string())
            .bind(&receipt.mailbox)
            .bind(&receipt.remote_name)
            .bind(i64::from(receipt.uid_validity))
            .bind(receipt.account_config_generation)
            .bind(&receipt.account_config_fingerprint)
            .bind(receipt.account_config_generation)
            .bind(&receipt.account_config_fingerprint)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(updated == 1)
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
            // Keep the old namespace readable while its replacement is
            // staged. Fenced receipt writes reject the old locator as soon as
            // the replacement exists, and final publication removes the old
            // rows and their dependent caches atomically.
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

    /// Captures a content-fetch receipt under the currently committed mailbox
    /// namespace. Call this before opening IMAP, then pass it to the fenced
    /// content commit methods below.
    pub async fn capture_message_remote_identity(
        &self,
        message_id: &str,
    ) -> Result<Option<MessageRemoteIdentity>> {
        Ok(sqlx::query_as("SELECT message.id AS message_id, message.account_id, account.config_generation AS account_config_generation, account.config_fingerprint AS account_config_fingerprint, message.mailbox, catalogue.remote_name, message.uid, catalogue.uid_validity FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.id = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Resolves the persisted local id for one exact committed IMAP locator.
    /// UID numbers are only meaningful inside UIDVALIDITY, so callers that
    /// receive a later FETCH result must use this instead of reconstructing a
    /// legacy account/mailbox/UID id.
    pub async fn canonical_message_id_for_locator(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
        uid_validity: u32,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT message.id FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.account_id = ? AND message.mailbox = ? AND message.uid = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != catalogue.uid_validity)")
            .bind(account_id.to_string()).bind(mailbox).bind(i64::from(uid)).bind(i64::from(uid_validity))
            .fetch_optional(&self.pool).await?)
    }

    pub async fn message_remote_identity_is_current(
        &self,
        identity: &MessageRemoteIdentity,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.id = ? AND message.account_id = ? AND account.config_generation = ? AND account.config_fingerprint = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&identity.message_id).bind(&identity.account_id).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(&identity.mailbox).bind(identity.uid).bind(&identity.remote_name).bind(identity.uid_validity)
            .fetch_one(&self.pool).await?)
    }

    pub async fn set_message_content_state_if_current(
        &self,
        identity: &MessageRemoteIdentity,
        state: &str,
    ) -> Result<bool> {
        if !matches!(state, "headers_only" | "hydrating" | "complete" | "failed") {
            return Err(anyhow!("invalid message content state"));
        }
        let updated = sqlx::query("UPDATE messages SET content_state = ? WHERE id = ? AND account_id = ? AND mailbox = ? AND uid = ? AND EXISTS (SELECT 1 FROM accounts AS account WHERE account.id = messages.account_id AND account.config_generation = ? AND account.config_fingerprint = ?) AND EXISTS (SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = messages.account_id AND catalogue.mailbox = messages.mailbox AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = ? AND catalogue.provider_config_fingerprint = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = messages.account_id AND replacement.mailbox = messages.mailbox AND replacement.uid_validity != ?)")
            .bind(state).bind(&identity.message_id).bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(&identity.remote_name).bind(identity.uid_validity).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(identity.uid_validity)
            .execute(&self.pool).await?.rows_affected();
        Ok(updated == 1)
    }

    pub async fn claim_message_hydration(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE messages SET content_state = 'hydrating' WHERE id = ? AND content_state IN ('headers_only', 'failed')")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn message(&self, id: &str) -> Result<Option<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id = ?";
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
        let mut tx = self.pool.begin().await?;
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
        sqlx::query("UPDATE messages SET is_read = ? WHERE id = ?")
            .bind(read)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn starred_attachment_metadata(&self, message_id: &str) -> Result<Vec<Attachment>> {
        Ok(sqlx::query_as::<_, Attachment>("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, presentation, is_potentially_unsafe FROM starred_attachment_metadata WHERE message_id = ? AND presentation IN ('downloadable', 'both') ORDER BY filename, id")
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Claims one operation before any SMTP or IMAP side effect. The immediate
    /// transaction makes the state transition a compare-and-swap, preventing
    /// two workers from submitting the same queued message or APPEND.
    pub async fn claim_pending_operation(
        &self,
        account_id: AccountId,
        claim_owner: &str,
    ) -> Result<Option<OperationJournalEntry>> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        let account_id = account_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let candidate: Option<OperationJournalEntry> = sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal AS operation WHERE account_id = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones AS removed WHERE removed.account_id = operation.account_id) AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = operation.account_id AND removal.expires_at > ?) AND operation.state IN ('queued', 'retry', 'sent_copy_pending') AND (operation.kind <> 'smtp_submission' OR NOT EXISTS (SELECT 1 FROM operation_journal AS active_smtp WHERE active_smtp.account_id = operation.account_id AND active_smtp.kind = 'smtp_submission' AND active_smtp.state = 'submitting')) AND (operation.next_retry_at IS NULL OR operation.next_retry_at <= ?) AND (operation.dependency_id IS NULL OR EXISTS (SELECT 1 FROM operation_journal AS dependency WHERE dependency.operation_id = operation.dependency_id AND dependency.state = 'completed')) AND NOT EXISTS (SELECT 1 FROM operation_journal AS earlier WHERE earlier.account_id = operation.account_id AND earlier.mailbox = operation.mailbox AND earlier.uid = operation.uid AND earlier.local_version < operation.local_version AND earlier.state NOT IN ('completed', 'rejected', 'permanent_failed', 'uncertain')) ORDER BY COALESCE(operation.mailbox, ''), operation.local_version, operation.created_at LIMIT 1")
            .bind(&account_id).bind(Utc::now()).bind(Utc::now()).fetch_optional(&mut *tx).await?;
        let Some(candidate) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        let claimed = sqlx::query("UPDATE operation_journal SET state = 'submitting', claim_owner = ?, claimed_at = ?, claimed_from_state = state, attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND state = ? AND claim_owner IS NULL AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones AS removed WHERE removed.account_id = operation_journal.account_id) AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = operation_journal.account_id AND removal.expires_at > ?)")
            .bind(claim_owner).bind(Utc::now()).bind(Utc::now()).bind(&candidate.operation_id).bind(&candidate.state).bind(Utc::now())
            .execute(&mut *tx).await?;
        if claimed.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(None);
        }
        tx.commit().await?;
        let operation = self
            .operation_journal_entry(&candidate.operation_id)
            .await?;
        if let Some(operation) = operation.as_ref() {
            Self::record_first_smtp_claim_wait(operation);
        }
        Ok(operation)
    }

    /// Claims only a known Sent-copy operation. This is deliberately narrower
    /// than the generic queue drain: recovery must never auto-submit a queued
    /// or ambiguous SMTP message while trying to reconcile its Sent copy.
    pub async fn claim_sent_copy_operation(
        &self,
        operation_id: &str,
        claim_owner: &str,
    ) -> Result<Option<OperationJournalEntry>> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let claimed = sqlx::query("UPDATE operation_journal SET state = 'submitting', claim_owner = ?, claimed_at = ?, claimed_from_state = state, attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND (state = 'sent_copy_pending' OR (state = 'retry' AND outcome = 'sent_copy_retry_scheduled')) AND claim_owner IS NULL AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones AS removed WHERE removed.account_id = operation_journal.account_id) AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = operation_journal.account_id AND removal.expires_at > ?)")
            .bind(claim_owner).bind(Utc::now()).bind(Utc::now()).bind(operation_id).bind(Utc::now())
            .execute(&mut *tx).await?;
        if claimed.rows_affected() != 1 {
            tx.commit().await?;
            return Ok(None);
        }
        tx.commit().await?;
        self.operation_journal_entry(operation_id).await
    }

    /// Claims only a provider-side Sent reconciliation already known to have
    /// accepted SMTP delivery. It cannot accidentally submit a queued send.
    pub async fn claim_provider_sent_operation(
        &self,
        operation_id: &str,
        claim_owner: &str,
    ) -> Result<Option<OperationJournalEntry>> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        let claimed = sqlx::query("UPDATE operation_journal SET state = 'submitting', claim_owner = ?, claimed_at = ?, claimed_from_state = 'accepted', attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND state = 'accepted' AND outcome = 'provider_sent_reconciliation' AND claim_owner IS NULL AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones AS removed WHERE removed.account_id = operation_journal.account_id) AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = operation_journal.account_id AND removal.expires_at > ?)")
            .bind(claim_owner).bind(Utc::now()).bind(Utc::now()).bind(operation_id).bind(Utc::now())
            .execute(&self.pool).await?;
        if claimed.rows_affected() != 1 {
            return Ok(None);
        }
        self.operation_journal_entry(operation_id).await
    }

    /// Claims one explicitly selected SMTP submission. It never drains an
    /// unrelated queue item and only permits a known safe pre-submit state.
    pub async fn claim_operation_by_id(
        &self,
        operation_id: &str,
        claim_owner: &str,
    ) -> Result<Option<OperationJournalEntry>> {
        if claim_owner.trim().is_empty() {
            return Err(anyhow!("operation claim owner is required"));
        }
        let updated = sqlx::query("UPDATE operation_journal SET state = 'submitting', claim_owner = ?, claimed_at = ?, claimed_from_state = state, attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND state IN ('queued', 'retry') AND claim_owner IS NULL AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones AS removed WHERE removed.account_id = operation_journal.account_id) AND NOT EXISTS (SELECT 1 FROM account_removal_gates AS removal WHERE removal.account_id = operation_journal.account_id AND removal.expires_at > ?) AND (kind <> 'smtp_submission' OR (NOT EXISTS (SELECT 1 FROM operation_journal AS active_smtp WHERE active_smtp.account_id = operation_journal.account_id AND active_smtp.kind = 'smtp_submission' AND active_smtp.state = 'submitting') AND NOT EXISTS (SELECT 1 FROM operation_journal AS earlier_smtp WHERE earlier_smtp.account_id = operation_journal.account_id AND earlier_smtp.kind = 'smtp_submission' AND earlier_smtp.smtp_accepted_at IS NULL AND earlier_smtp.state NOT IN ('accepted', 'completed', 'rejected', 'permanent_failed', 'uncertain') AND (earlier_smtp.created_at < operation_journal.created_at OR (earlier_smtp.created_at = operation_journal.created_at AND earlier_smtp.operation_id < operation_journal.operation_id)))))")
            .bind(claim_owner).bind(Utc::now()).bind(Utc::now()).bind(operation_id).bind(Utc::now())
            .execute(&self.pool).await?;
        if updated.rows_affected() != 1 {
            return Ok(None);
        }
        let operation = self.operation_journal_entry(operation_id).await?;
        if let Some(operation) = operation.as_ref() {
            Self::record_first_smtp_claim_wait(operation);
        }
        Ok(operation)
    }

    fn record_first_smtp_claim_wait(operation: &OperationJournalEntry) {
        if operation.kind == "smtp_submission"
            && operation.state == "submitting"
            && operation.attempts == 1
            && operation.smtp_accepted_at.is_none()
        {
            crate::mail_metrics::record_submission_queue_wait(
                Utc::now()
                    .signed_duration_since(operation.created_at)
                    .to_std()
                    .unwrap_or_default(),
            );
        }
    }

    /// Lists currently leased SMTP submissions for account-removal
    /// coordination. The caller may wait for these known operations to reach
    /// a durable outcome before removing credentials; this read never claims
    /// or changes their delivery state.
    pub async fn claimed_smtp_operations(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, created_at, updated_at FROM operation_journal WHERE account_id = ? AND kind = 'smtp_submission' AND state = 'submitting' ORDER BY claimed_at, operation_id")
            .bind(account_id.to_string())
            .fetch_all(&self.pool)
            .await?)
    }

    /// Lists every active provider lease for one account. Account updates and
    /// removal must wait for mailbox actions as well as SMTP, otherwise an
    /// old connection can finish after the account settings change.
    pub async fn claimed_account_operations(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<OperationJournalEntry>> {
        Ok(sqlx::query_as("SELECT operation_id, account_id, mailbox, uid, uid_validity, message_id, kind, payload_json, local_version, dependency_id, state, outcome, attempts, next_retry_at, error, smtp_accepted_at, claim_owner, claimed_at, created_at, updated_at FROM operation_journal WHERE account_id = ? AND state = 'submitting' ORDER BY claimed_at, operation_id")
            .bind(account_id.to_string())
            .fetch_all(&self.pool)
            .await?)
    }

    /// Renews a live operation lease while a worker is inside an unbounded
    /// provider call. Recovery only touches claims whose heartbeat has
    /// expired, so another process cannot steal an active CLI submission.
    pub async fn renew_operation_claim(
        &self,
        operation_id: &str,
        claim_owner: &str,
    ) -> Result<bool> {
        Ok(sqlx::query("UPDATE operation_journal SET claimed_at = ?, updated_at = ? WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ?")
            .bind(Utc::now())
            .bind(Utc::now())
            .bind(operation_id)
            .bind(claim_owner)
            .execute(&self.pool)
            .await?
            .rows_affected() == 1)
    }

    pub fn heartbeat_operation_claim(
        &self,
        operation_id: impl Into<String>,
        claim_owner: impl Into<String>,
    ) -> OperationClaimHeartbeat {
        let store = self.clone();
        let operation_id = operation_id.into();
        let claim_owner = claim_owner.into();
        let task = tokio::spawn(async move {
            let interval =
                Duration::from_secs(u64::try_from(OPERATION_CLAIM_LEASE_SECONDS / 3).unwrap_or(30));
            loop {
                tokio::time::sleep(interval).await;
                match store
                    .renew_operation_claim(&operation_id, &claim_owner)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        });
        OperationClaimHeartbeat { task }
    }

    pub async fn next_operation_claim_expiry_at(
        &self,
        account_id: AccountId,
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT MIN(claimed_at) FROM operation_journal WHERE account_id = ? AND state = 'submitting' AND claimed_at IS NOT NULL")
            .bind(account_id.to_string()).fetch_one(&self.pool).await?
            .map(|claimed_at| claimed_at + chrono::Duration::seconds(OPERATION_CLAIM_LEASE_SECONDS)))
    }

    /// Completes only the lease owner which performed the remote attempt.
    /// `uncertain` intentionally retains the lease/fence for reconciliation
    /// instead of allowing an automatic resend after a process crash.
    pub async fn complete_claimed_operation(
        &self,
        operation_id: &str,
        claim_owner: &str,
        state: &str,
        outcome: Option<&str>,
        error: Option<&str>,
        next_retry_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        if !matches!(
            state,
            "retry"
                | "accepted"
                | "sent_copy_pending"
                | "completed"
                | "rejected"
                | "permanent_failed"
                | "uncertain"
        ) {
            return Err(anyhow!("invalid claimed operation state"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now();
        let changed = sqlx::query("UPDATE operation_journal SET state = ?, outcome = ?, error = ?, next_retry_at = ?, smtp_accepted_at = CASE WHEN ? = 'accepted' OR (? = 'sent_copy_pending' AND kind = 'smtp_submission') THEN COALESCE(smtp_accepted_at, ?) ELSE smtp_accepted_at END, claim_owner = CASE WHEN ? = 'uncertain' THEN claim_owner ELSE NULL END, claimed_at = CASE WHEN ? = 'uncertain' THEN claimed_at ELSE NULL END, claimed_from_state = CASE WHEN ? = 'uncertain' THEN claimed_from_state ELSE NULL END, updated_at = ? WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ?")
            .bind(state).bind(outcome).bind(error).bind(next_retry_at).bind(state).bind(state).bind(now).bind(state).bind(state).bind(state).bind(now).bind(operation_id).bind(claim_owner)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!("operation claim is stale or missing"));
        }
        if matches!(state, "completed" | "rejected" | "permanent_failed") {
            sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
                .bind(operation_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Atomically projects a provider-confirmed move/delete and terminalizes
    /// the exact journal lease. Workers must call this after the remote OK;
    /// it closes the crash window between local membership reconciliation and
    /// releasing the operation fence.
    pub async fn reconcile_and_complete_claimed_mailbox_action<D>(
        &self,
        operation_id: &str,
        claim_owner: &str,
        destination_mailbox: &str,
        destination: Option<D>,
    ) -> Result<bool>
    where
        D: Into<MoveDestinationLocator>,
    {
        let destination = destination.map(Into::into);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let operation: Option<(String, String, i64, i64, String)> = sqlx::query_as("SELECT account_id, mailbox, uid, uid_validity, message_id FROM operation_journal WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ? AND kind = 'mailbox_action' AND message_id IS NOT NULL")
            .bind(operation_id).bind(claim_owner).fetch_optional(&mut *tx).await?;
        let Some((account_id, source_mailbox, source_uid, uid_validity, message_id)) = operation
        else {
            tx.commit().await?;
            return Ok(false);
        };
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? AND uid_validity = ?)")
            .bind(&account_id).bind(&source_mailbox).bind(uid_validity).fetch_one(&mut *tx).await?;
        if !current {
            tx.rollback().await?;
            return Ok(false);
        }
        let pending_mailbox = format!("__pending_action__:{operation_id}");
        let source_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id = ? AND account_id = ? AND mailbox IN (?, ?))")
            .bind(&message_id)
            .bind(&account_id)
            .bind(&source_mailbox)
            .bind(&pending_mailbox)
            .fetch_one(&mut *tx)
            .await?;
        if !source_exists {
            tx.rollback().await?;
            return Err(anyhow!("mailbox action source row is missing"));
        }
        sqlx::query("INSERT OR REPLACE INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, ?, ?, ?)")
            .bind(&account_id).bind(&source_mailbox).bind(source_uid).bind(Utc::now()).execute(&mut *tx).await?;
        if let Some(destination) = destination {
            let destination_uid = destination.uid;
            // Only the legacy `u32` compatibility conversion uses zero as an
            // unspecified namespace. New provider COPYUID callers always
            // supply a non-zero UIDVALIDITY and never take this branch.
            let destination_uid_validity = if destination.uid_validity == 0 {
                sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
                )
                .bind(&account_id)
                .bind(destination_mailbox)
                .fetch_one(&mut *tx)
                .await?
                .and_then(|value| u32::try_from(value).ok())
            } else {
                Some(destination.uid_validity)
            };
            for table in [
                "message_content_cache",
                "starred_attachment_metadata",
                "starred_message_bodies",
                "message_content_fetches",
            ] {
                let statement = format!("DELETE FROM {table} WHERE message_id = ?");
                sqlx::query(&statement)
                    .bind(&message_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
                .bind(&account_id)
                .bind(destination_mailbox)
                .bind(i64::from(destination_uid))
                .execute(&mut *tx)
                .await?;
            let destination_namespace_current = destination_uid_validity.is_some_and(|uid_validity| {
                // The exact namespace is rechecked in this same write
                // transaction before a locally addressable destination row.
                destination.uid_validity == 0 || destination.uid_validity == uid_validity
            }) && sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? AND uid_validity = ?)")
                .bind(&account_id).bind(destination_mailbox).bind(i64::from(destination_uid_validity.unwrap())).fetch_one(&mut *tx).await?;
            if !destination_namespace_current {
                // COPYUID is authoritative about the remote move, but this
                // local catalogue has not selected that exact namespace yet.
                // Delete the source projection and let later authenticated
                // destination sync create its UIDVALIDITY-scoped row.
                let deleted = sqlx::query(
                    "DELETE FROM messages WHERE id = ? AND account_id = ? AND mailbox IN (?, ?)",
                )
                .bind(&message_id)
                .bind(&account_id)
                .bind(&source_mailbox)
                .bind(&pending_mailbox)
                .execute(&mut *tx)
                .await?;
                if deleted.rows_affected() != 1 {
                    tx.rollback().await?;
                    return Err(anyhow!(
                        "mailbox action source row changed before reconciliation"
                    ));
                }
            } else {
                let destination_id = uidvalidity_message_id(
                    AccountId::parse_str(&account_id)?,
                    destination_mailbox,
                    destination_uid,
                    destination_uid_validity.expect("current destination namespace"),
                );
                let moved = sqlx::query("UPDATE messages SET id = ?, mailbox = ?, uid = ? WHERE id = ? AND account_id = ? AND mailbox IN (?, ?)")
                .bind(destination_id).bind(destination_mailbox).bind(i64::from(destination_uid)).bind(&message_id).bind(&account_id).bind(&source_mailbox).bind(&pending_mailbox).execute(&mut *tx).await?;
                if moved.rows_affected() != 1 {
                    tx.rollback().await?;
                    return Err(anyhow!(
                        "mailbox action source row changed before reconciliation"
                    ));
                }
            }
        } else {
            let deleted = sqlx::query(
                "DELETE FROM messages WHERE id = ? AND account_id = ? AND mailbox IN (?, ?)",
            )
            .bind(&message_id)
            .bind(&account_id)
            .bind(&source_mailbox)
            .bind(&pending_mailbox)
            .execute(&mut *tx)
            .await?;
            if deleted.rows_affected() != 1 {
                tx.rollback().await?;
                return Err(anyhow!(
                    "mailbox action source row changed before reconciliation"
                ));
            }
        }
        let completed = sqlx::query("UPDATE operation_journal SET state = 'completed', outcome = 'remote_reconciled', error = NULL, next_retry_at = NULL, claim_owner = NULL, claimed_at = NULL, claimed_from_state = NULL, updated_at = ? WHERE operation_id = ? AND state = 'submitting' AND claim_owner = ?")
            .bind(Utc::now()).bind(operation_id).bind(claim_owner).execute(&mut *tx).await?;
        if completed.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(anyhow!("operation claim is stale or missing"));
        }
        sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM operation_message_backups WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Startup recovery converts an interrupted transport attempt into an
    /// explicit reconciliation state. It never puts it back in the send queue.
    pub async fn mark_interrupted_operations_uncertain(
        &self,
        account_id: AccountId,
    ) -> Result<u64> {
        let now = Utc::now();
        Ok(sqlx::query("UPDATE operation_journal SET state = CASE WHEN kind IN ('message_read', 'message_star') THEN 'retry' WHEN claimed_from_state = 'accepted' AND outcome = 'provider_sent_reconciliation' THEN 'accepted' WHEN claimed_from_state = 'retry' AND outcome = 'sent_copy_retry_scheduled' THEN 'retry' ELSE 'uncertain' END, outcome = CASE WHEN kind IN ('message_read', 'message_star') THEN 'idempotent_retry_after_interruption' WHEN claimed_from_state = 'accepted' AND outcome = 'provider_sent_reconciliation' THEN 'provider_sent_reconciliation' WHEN claimed_from_state = 'retry' AND outcome = 'sent_copy_retry_scheduled' THEN 'sent_copy_retry_scheduled' WHEN claimed_from_state = 'sent_copy_pending' THEN 'smtp_accepted_sent_copy_uncertain' ELSE COALESCE(outcome, 'interrupted_before_outcome') END, error = CASE WHEN kind IN ('message_read', 'message_star') THEN COALESCE(error, 'idempotent flag update interrupted before durable outcome') WHEN claimed_from_state = 'accepted' AND outcome = 'provider_sent_reconciliation' THEN COALESCE(error, 'SMTP accepted; provider Sent reconciliation was interrupted') WHEN claimed_from_state IN ('sent_copy_pending', 'retry') THEN COALESCE(error, 'SMTP accepted; Sent-copy operation was interrupted') ELSE COALESCE(error, 'operation interrupted before provider outcome') END, next_retry_at = CASE WHEN kind IN ('message_read', 'message_star') THEN ? ELSE next_retry_at END, claim_owner = NULL, claimed_at = NULL, claimed_from_state = NULL, updated_at = ? WHERE account_id = ? AND state = 'submitting' AND (claimed_at IS NULL OR claimed_at <= ?)")
            .bind(now + chrono::Duration::seconds(1)).bind(now).bind(account_id.to_string()).bind(now - chrono::Duration::seconds(OPERATION_CLAIM_LEASE_SECONDS)).execute(&self.pool).await?.rows_affected())
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

    pub async fn update_message_attachment_state_if_current(
        &self,
        identity: &MessageRemoteIdentity,
        has_attachments: bool,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE messages SET has_attachments = ? WHERE id = ? AND account_id = ? AND mailbox = ? AND uid = ? AND EXISTS (SELECT 1 FROM accounts AS account WHERE account.id = messages.account_id AND account.config_generation = ? AND account.config_fingerprint = ?) AND EXISTS (SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = messages.account_id AND catalogue.mailbox = messages.mailbox AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = ? AND catalogue.provider_config_fingerprint = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = messages.account_id AND replacement.mailbox = messages.mailbox AND replacement.uid_validity != ?)")
            .bind(has_attachments).bind(&identity.message_id).bind(&identity.account_id).bind(&identity.mailbox).bind(identity.uid).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(&identity.remote_name).bind(identity.uid_validity).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(identity.uid_validity)
            .execute(&self.pool).await?.rows_affected();
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
        self.cache_starred_message_content_inner(message_id, content, None)
            .await
    }

    pub async fn cache_starred_message_content_if_current(
        &self,
        identity: &MessageRemoteIdentity,
        content: CachedMessageContent,
    ) -> Result<bool> {
        self.cache_starred_message_content_inner(&identity.message_id, content, Some(identity))
            .await
    }

    async fn cache_starred_message_content_inner(
        &self,
        message_id: &str,
        content: CachedMessageContent,
        identity: Option<&MessageRemoteIdentity>,
    ) -> Result<bool> {
        let mut attachments = content.attachments;
        for attachment in &mut attachments {
            attachment.message_id = message_id.to_owned();
        }
        attachments.retain(|attachment| attachment.presentation.is_downloadable());

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !Self::cached_content_receipt_is_current(&mut tx, identity).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        let still_flagged: Option<bool> =
            sqlx::query_scalar("SELECT is_flagged FROM messages WHERE id = ? AND (? IS NULL OR (account_id = ? AND mailbox = ? AND uid = ? AND EXISTS (SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = messages.account_id AND catalogue.mailbox = messages.mailbox AND catalogue.remote_name = ? AND catalogue.uid_validity = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = messages.account_id AND replacement.mailbox = messages.mailbox AND replacement.uid_validity != ?)))")
                .bind(message_id)
                .bind(identity.map(|_| 1_i64))
                .bind(identity.map(|identity| identity.account_id.as_str()))
                .bind(identity.map(|identity| identity.mailbox.as_str()))
                .bind(identity.map(|identity| identity.uid))
                .bind(identity.map(|identity| identity.remote_name.as_str()))
                .bind(identity.map(|identity| identity.uid_validity))
                .bind(identity.map(|identity| identity.uid_validity))
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
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn cache_message_content_if_current(
        &self,
        identity: &MessageRemoteIdentity,
        is_flagged: bool,
        content: CachedMessageContent,
    ) -> Result<bool> {
        self.cache_message_content_with_budget(
            &identity.message_id,
            is_flagged,
            content,
            MESSAGE_CONTENT_CACHE_MAX_BYTES,
            Some(identity),
        )
        .await
    }

    async fn cache_message_content_with_budget(
        &self,
        message_id: &str,
        is_flagged: bool,
        content: CachedMessageContent,
        max_bytes: i64,
        identity: Option<&MessageRemoteIdentity>,
    ) -> Result<bool> {
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !Self::cached_content_receipt_is_current(&mut tx, identity).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        if byte_size > max_bytes {
            sqlx::query("DELETE FROM message_content_cache WHERE message_id = ? AND (? IS NULL OR EXISTS (SELECT 1 FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox WHERE message.id = ? AND message.account_id = ? AND message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != ?)))")
                .bind(message_id)
                .bind(identity.map(|_| 1_i64))
                .bind(message_id)
                .bind(identity.map(|identity| identity.account_id.as_str()))
                .bind(identity.map(|identity| identity.mailbox.as_str()))
                .bind(identity.map(|identity| identity.uid))
                .bind(identity.map(|identity| identity.remote_name.as_str()))
                .bind(identity.map(|identity| identity.uid_validity))
                .bind(identity.map(|identity| identity.uid_validity))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(identity.is_none());
        }

        let stored = sqlx::query(
            "INSERT INTO message_content_cache(message_id, content_state, body_text, body_html, unsubscribe_kind, attachments_json, byte_size, last_accessed) SELECT ?, 'complete', ?, ?, ?, ?, ?, (SELECT COALESCE(MAX(last_accessed), 0) + 1 FROM message_content_cache) WHERE ? = 0 AND EXISTS (SELECT 1 FROM messages WHERE id = ? AND is_flagged = 0) AND (? IS NULL OR EXISTS (SELECT 1 FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox WHERE message.id = ? AND message.account_id = ? AND message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != ?))) ON CONFLICT(message_id) DO UPDATE SET content_state = excluded.content_state, body_text = excluded.body_text, body_html = excluded.body_html, unsubscribe_kind = excluded.unsubscribe_kind, attachments_json = excluded.attachments_json, byte_size = excluded.byte_size, last_accessed = excluded.last_accessed",
        )
        .bind(message_id)
        .bind(&content.body_text)
        .bind(&content.body_html)
        .bind(&content.unsubscribe_kind)
        .bind(&attachments_json)
        .bind(byte_size)
        .bind(is_flagged && identity.is_none())
        .bind(message_id)
        .bind(identity.map(|_| 1_i64))
        .bind(message_id)
        .bind(identity.map(|identity| identity.account_id.as_str()))
        .bind(identity.map(|identity| identity.mailbox.as_str()))
        .bind(identity.map(|identity| identity.uid))
        .bind(identity.map(|identity| identity.remote_name.as_str()))
        .bind(identity.map(|identity| identity.uid_validity))
        .bind(identity.map(|identity| identity.uid_validity))
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
            return Ok(false);
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
        Ok(true)
    }

    // Called only under BEGIN IMMEDIATE. In particular, stale oversized or
    // unstarred fetches must not delete a newer cache entry while rejecting
    // their own content. The guard covers every write in the transaction.
    async fn cached_content_receipt_is_current(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        identity: Option<&MessageRemoteIdentity>,
    ) -> Result<bool> {
        let Some(identity) = identity else {
            return Ok(true);
        };
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages AS message JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = message.account_id AND catalogue.mailbox = message.mailbox JOIN accounts AS account ON account.id = message.account_id WHERE message.id = ? AND message.account_id = ? AND account.config_generation = ? AND account.config_fingerprint = ? AND catalogue.provider_config_generation = account.config_generation AND catalogue.provider_config_fingerprint = account.config_fingerprint AND message.mailbox = ? AND message.uid = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = message.account_id AND replacement.mailbox = message.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&identity.message_id).bind(&identity.account_id).bind(identity.account_config_generation).bind(&identity.account_config_fingerprint).bind(&identity.mailbox).bind(identity.uid).bind(&identity.remote_name).bind(identity.uid_validity)
            .fetch_one(&mut **tx).await?)
    }

    /// Returns recent primary-folder messages that still need their body in
    /// the cache appropriate to their current flag state. Local message
    /// identity is the cache key: generic RFC Message-IDs can legitimately
    /// occur on unrelated provider messages and must not hide either one.
    pub async fn recent_body_cache_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<MailSummary>> {
        self.recent_body_cache_candidates_page(account_id, cutoff, limit, 0)
            .await
    }

    /// Returns a deterministic page of recent body-cache candidates.
    pub async fn recent_body_cache_candidates_page(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "WITH uncached AS (SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals, ROW_NUMBER() OVER (PARTITION BY m.id ORDER BY m.received_at DESC, m.id DESC) AS duplicate_rank FROM messages m LEFT JOIN message_content_cache c ON c.message_id = m.id LEFT JOIN starred_message_bodies b ON b.message_id = m.id AND b.attachment_presentation_version = ? WHERE m.account_id = ? AND m.mailbox IN ('INBOX', 'Sent', 'Archive') AND m.received_at >= ? AND ((m.is_flagged = 0 AND c.message_id IS NULL) OR (m.is_flagged = 1 AND b.message_id IS NULL))) SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM uncached WHERE duplicate_rank = 1 ORDER BY received_at DESC, id DESC LIMIT ? OFFSET ?";
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
            "INSERT INTO message_content_fetches(message_id, claimed_at, claim_owner) SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?) ON CONFLICT(message_id) DO UPDATE SET claimed_at = excluded.claimed_at, claim_owner = excluded.claim_owner WHERE message_content_fetches.claimed_at <= ? OR (excluded.claim_owner NOT LIKE 'background:%' AND message_content_fetches.claim_owner LIKE 'background:%')",
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
        self.acquire_message_content_fetch_with_owner(message_id, uuid::Uuid::new_v4().to_string())
            .await
    }

    /// A reader can immediately take over low-priority automatic warming.
    /// A late warmer cannot release the replacement reader's unique lease.
    pub async fn acquire_background_message_content_fetch(
        &self,
        message_id: &str,
    ) -> Result<Option<MessageContentFetchClaim>> {
        self.acquire_message_content_fetch_with_owner(
            message_id,
            format!("background:{}", uuid::Uuid::new_v4()),
        )
        .await
    }

    async fn acquire_message_content_fetch_with_owner(
        &self,
        message_id: &str,
        owner: String,
    ) -> Result<Option<MessageContentFetchClaim>> {
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
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND mailbox = 'INBOX' AND content_state != 'complete' ORDER BY received_at DESC LIMIT ?";
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
        const SQL: &str = "SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals FROM messages m LEFT JOIN starred_message_bodies b ON b.message_id = m.id WHERE m.account_id = ? AND m.is_flagged = 1 AND (b.message_id IS NULL OR b.attachment_presentation_version != ?) ORDER BY m.received_at DESC LIMIT ?";
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
        let destination_uid_validity: Option<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_key)
        .bind(destination_mailbox)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let destination_id = match destination_uid_validity {
            Some(uid_validity) => uidvalidity_message_id(
                account_id,
                destination_mailbox,
                destination_uid,
                u32::try_from(uid_validity)?,
            ),
            None => format!(
                "{}:pending:{}",
                stable_message_id(account_id, destination_mailbox, destination_uid),
                uuid::Uuid::new_v4()
            ),
        };
        sqlx::query("UPDATE messages SET id = ?, mailbox = ?, uid = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(destination_id)
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
        let destination_uid_validity: Option<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_key)
        .bind(destination_mailbox)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let destination_id = match destination_uid_validity {
            Some(uid_validity) => uidvalidity_message_id(
                account_id,
                destination_mailbox,
                destination_uid,
                u32::try_from(uid_validity)?,
            ),
            None => format!(
                "{}:pending:{}",
                stable_message_id(account_id, destination_mailbox, destination_uid),
                uuid::Uuid::new_v4()
            ),
        };
        sqlx::query("UPDATE messages SET id = ?, mailbox = ?, uid = ? WHERE account_id = ? AND mailbox = ? AND uid = ?")
            .bind(destination_id)
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

    /// Applies a provider-confirmed move only when COPYUID supplied the exact
    /// destination namespace. If the destination catalogue is not yet known,
    /// source tombstones are still recorded but no synthetic destination row
    /// is created; a later authenticated sync publishes that locator.
    pub async fn move_messages_to_destination_with_namespace(
        &self,
        account_id: AccountId,
        sources: &[(String, u32)],
        destination_mailbox: &str,
        destination: Option<MoveDestinationLocator>,
    ) -> Result<()> {
        let Some(destination) = destination else {
            return self
                .move_messages_to_destination(account_id, sources, destination_mailbox, None)
                .await;
        };
        let namespace_current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? AND uid_validity = ?)",
        )
        .bind(account_id.to_string())
        .bind(destination_mailbox)
        .bind(i64::from(destination.uid_validity))
        .fetch_one(&self.pool)
        .await?;
        self.move_messages_to_destination(
            account_id,
            sources,
            destination_mailbox,
            namespace_current.then_some(destination.uid),
        )
        .await
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<MailSummary>> {
        self.search_with_projection(
            query,
            "m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals",
        )
        .await
    }

    async fn search_with_projection(
        &self,
        query: &SearchQuery,
        projection: &str,
    ) -> Result<Vec<MailSummary>> {
        let limit = query.limit.unwrap_or(100).clamp(1, 500) as i64;
        let mut sql = format!("SELECT {projection} FROM messages m");
        if !query.text.trim().is_empty() {
            sql.push_str(" JOIN messages_fts f ON f.rowid=m.rowid");
        }
        sql.push_str(" WHERE 1=1");
        if !query.text.trim().is_empty() {
            sql.push_str(" AND messages_fts MATCH ?");
        }
        if !query.account_ids.is_empty() {
            sql.push_str(" AND m.account_id IN (");
            sql.push_str(&vec!["?"; query.account_ids.len()].join(","));
            sql.push(')');
        }
        if query.mailbox.is_some() {
            if query
                .mailbox
                .as_deref()
                .is_some_and(is_special_mailbox_family)
            {
                sql.push_str(" AND (m.mailbox = ? OR m.mailbox LIKE ?)");
            } else {
                sql.push_str(" AND m.mailbox = ?");
            }
        } else {
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
        if query.cursor.is_some() {
            sql.push_str(" AND (m.received_at < ? OR (m.received_at = ? AND m.id < ?))");
        }
        sql.push_str(" ORDER BY m.received_at DESC, m.id DESC LIMIT ?");

        let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
        if !query.text.trim().is_empty() {
            statement = statement.bind(fts_query(&query.text));
        }
        for account_id in &query.account_ids {
            statement = statement.bind(account_id.to_string());
        }
        if let Some(mailbox) = &query.mailbox {
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
        if let Some(cursor) = &query.cursor {
            statement = statement
                .bind(cursor.received_at)
                .bind(cursor.received_at)
                .bind(&cursor.id);
        }
        Ok(statement.bind(limit).fetch_all(&self.pool).await?)
    }

    /// Finds conversations by messages matching the requested view, then
    /// hydrates their allowed account-wide members. This intentionally keeps
    /// mailbox membership separate from reader membership.
    pub async fn search_conversations(&self, query: &SearchQuery) -> Result<Vec<MailConversation>> {
        Ok(self.search_conversation_page(query).await?.conversations)
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
            let candidates = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND LOWER(TRIM(message_id)) = ?")
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
            let exists = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND thread_id = ? ORDER BY received_at, id")
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
        let mut source_messages = sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, '' AS body_text, NULL AS body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, '' AS classification_signals FROM messages WHERE account_id = ? AND thread_id = ? ORDER BY received_at, id")
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
        let limit = query.limit.unwrap_or(100).clamp(1, 500);
        let mut match_query = query.clone();
        // Select one representative matching message per conversation before
        // paging. Paging raw messages and grouping afterwards makes a thread
        // straddle pages, causing duplicates and unreliable `hasMore`.
        match_query.limit = Some(limit.saturating_add(1));
        let mut matching = self.search_conversation_matches(&match_query).await?;
        let has_more = matching.len() > limit as usize;
        matching.truncate(limit as usize);
        let next_cursor = has_more.then(|| {
            let last = matching
                .last()
                .expect("a page with more results contains a cursor source");
            MailCursor {
                received_at: last.received_at,
                id: last.id.clone(),
            }
        });
        let isolated = query
            .mailbox
            .as_deref()
            .is_some_and(|mailbox| matches!(mailbox, "Spam" | "Trash"));
        if query.mailbox.is_none() {
            matching
                .retain(|message| !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"));
        }

        let keys = matching
            .iter()
            .map(|message| (message.account_id.clone(), message.thread_id.clone()))
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(MailConversationPage {
                conversations: Vec::new(),
                next_cursor: None,
            });
        }

        let mut hydrated = Vec::new();
        // Keep well below SQLite's conservative parameter limit while avoiding
        // one hydration query per conversation.
        for chunk in keys.chunks(300) {
            let predicates = vec!["(account_id = ? AND thread_id = ?)"; chunk.len()].join(" OR ");
            let sql = format!("SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, '' AS body_text, NULL AS body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, '' AS classification_signals FROM messages m WHERE ({predicates}) ORDER BY m.received_at, m.id");
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
        Ok(MailConversationPage {
            conversations: keys
                .into_iter()
                .filter_map(|key| conversations.get(&key).cloned())
                .collect(),
            next_cursor,
        })
    }

    /// Loads the initial Smart Inbox sections in one SQLite statement and
    /// hydrates the union of their selected conversations once. Section
    /// pagination continues through `search_conversation_page` so cursors keep
    /// the same public meaning after the initial page.
    pub async fn search_smart_inbox(&self, query: &SmartInboxQuery) -> Result<SmartInboxPage> {
        const SECTION_IDS: [&str; 8] = [
            "starred",
            "unsorted",
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
                SELECT m.id, m.account_id, m.thread_id, m.received_at, m.category, m.is_read,
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
                SELECT scoped.id, scoped.account_id, scoped.thread_id, scoped.received_at, scoped.category, scoped.is_read, scoped.any_unread,
                    COALESCE(thread_flags.any_flagged, 0) AS any_flagged
                FROM scoped
                LEFT JOIN thread_flags USING (account_id, thread_id)
                WHERE scoped.thread_rank = 1
            ), sectioned AS (
                SELECT 'starred' AS section_id, id, account_id, thread_id, received_at FROM representatives WHERE any_flagged = 1
                UNION ALL
                SELECT 'unsorted' AS section_id, id, account_id, thread_id, received_at FROM representatives
                    WHERE any_flagged = 0 AND is_read = 0 AND category IS NULL
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
            .hydrate_conversations_by_keys(&keys, Some("INBOX"))
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
    ) -> Result<HashMap<(String, String), MailConversation>> {
        let mut hydrated = Vec::new();
        for chunk in keys.chunks(300) {
            let predicates = vec!["(account_id = ? AND thread_id = ?)"; chunk.len()].join(" OR ");
            let sql = format!("SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, '' AS body_text, NULL AS body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, '' AS classification_signals FROM messages m WHERE ({predicates}) ORDER BY m.received_at, m.id");
            let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
            for (account_id, thread_id) in chunk {
                statement = statement.bind(account_id).bind(thread_id);
            }
            hydrated.extend(statement.fetch_all(&self.pool).await?);
        }
        hydrated.retain(|message| !matches!(mailbox_family(&message.mailbox), "Spam" | "Trash"));
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

    async fn search_conversation_matches(&self, query: &SearchQuery) -> Result<Vec<MailSummary>> {
        // Conversation pages request one look-ahead candidate, so this
        // internal query intentionally accepts 501 while the public page size
        // remains capped at 500.
        let limit = query.limit.unwrap_or(100).clamp(1, 501) as i64;
        let projection = "m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, '' AS body_text, NULL AS body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, '' AS classification_signals";
        let mut sql = format!("WITH matching AS (SELECT {projection}, ROW_NUMBER() OVER (PARTITION BY m.account_id, m.thread_id ORDER BY m.received_at DESC, m.id DESC) AS thread_rank FROM messages m");
        if !query.text.trim().is_empty() {
            sql.push_str(" JOIN messages_fts f ON f.rowid=m.rowid");
        }
        sql.push_str(" WHERE 1=1");
        if !query.text.trim().is_empty() {
            sql.push_str(" AND messages_fts MATCH ?");
        }
        if !query.account_ids.is_empty() {
            sql.push_str(" AND m.account_id IN (");
            sql.push_str(&vec!["?"; query.account_ids.len()].join(","));
            sql.push(')');
        }
        if query.mailbox.is_some() {
            if query
                .mailbox
                .as_deref()
                .is_some_and(is_special_mailbox_family)
            {
                sql.push_str(" AND (m.mailbox = ? OR m.mailbox LIKE ?)");
            } else {
                sql.push_str(" AND m.mailbox = ?");
            }
        } else {
            sql.push_str(" AND m.mailbox NOT IN ('Spam', 'Trash') AND m.mailbox NOT LIKE 'Spam::%' AND m.mailbox NOT LIKE 'Trash::%'");
        }
        if query.from.is_some() {
            sql.push_str(" AND m.from_address LIKE ?");
        }
        if query.unflagged_only {
            sql.push_str(" AND NOT EXISTS (SELECT 1 FROM messages flagged WHERE flagged.account_id = m.account_id AND flagged.thread_id = m.thread_id AND flagged.is_flagged = 1)");
        }
        sql.push_str(") SELECT matching.* FROM matching WHERE thread_rank = 1");
        // Category is a conversation property in Smart views: use the latest
        // scoped mailbox representative, rather than allowing an older row to
        // put the same conversation in a second category.
        if query.category.is_some() {
            sql.push_str(" AND category = ?");
        }
        // Starred membership belongs to the conversation, but its ordering
        // and continuation must use the newest scoped representative.
        if query.flagged_only {
            sql.push_str(" AND EXISTS (SELECT 1 FROM messages flagged WHERE flagged.account_id = matching.account_id AND flagged.thread_id = matching.thread_id AND flagged.is_flagged = 1)");
        }
        // A conversation remains unread when any member in the mailbox scope
        // is unread, even if its latest representative has already been read.
        if query.unread_only {
            sql.push_str(" AND EXISTS (SELECT 1 FROM messages unread WHERE unread.account_id = matching.account_id AND unread.thread_id = matching.thread_id AND unread.is_read = 0");
            if let Some(mailbox) = query.mailbox.as_deref() {
                if is_special_mailbox_family(mailbox) {
                    sql.push_str(" AND (unread.mailbox = ? OR unread.mailbox LIKE ?)");
                } else {
                    sql.push_str(" AND unread.mailbox = ?");
                }
            } else {
                sql.push_str(" AND unread.mailbox NOT IN ('Spam', 'Trash') AND unread.mailbox NOT LIKE 'Spam::%' AND unread.mailbox NOT LIKE 'Trash::%'");
            }
            sql.push(')');
        }
        // A seen conversation has no unread member in the mailbox scope.
        if query.read_only {
            sql.push_str(" AND NOT EXISTS (SELECT 1 FROM messages unread WHERE unread.account_id = matching.account_id AND unread.thread_id = matching.thread_id AND unread.is_read = 0");
            if let Some(mailbox) = query.mailbox.as_deref() {
                if is_special_mailbox_family(mailbox) {
                    sql.push_str(" AND (unread.mailbox = ? OR unread.mailbox LIKE ?)");
                } else {
                    sql.push_str(" AND unread.mailbox = ?");
                }
            } else {
                sql.push_str(" AND unread.mailbox NOT IN ('Spam', 'Trash') AND unread.mailbox NOT LIKE 'Spam::%' AND unread.mailbox NOT LIKE 'Trash::%'");
            }
            sql.push(')');
        }
        if query.cursor.is_some() {
            sql.push_str(" AND (received_at < ? OR (received_at = ? AND id < ?))");
        }
        sql.push_str(" ORDER BY received_at DESC, id DESC LIMIT ?");

        let mut statement = sqlx::query_as::<_, MailSummary>(&sql);
        if !query.text.trim().is_empty() {
            statement = statement.bind(fts_query(&query.text));
        }
        for account_id in &query.account_ids {
            statement = statement.bind(account_id.to_string());
        }
        if let Some(mailbox) = &query.mailbox {
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
        if query.unread_only || query.read_only {
            if let Some(mailbox) = query.mailbox.as_deref() {
                statement = statement.bind(mailbox);
                if is_special_mailbox_family(mailbox) {
                    statement = statement.bind(format!("{mailbox}::%"));
                }
            }
        }
        if let Some(cursor) = &query.cursor {
            statement = statement
                .bind(cursor.received_at)
                .bind(cursor.received_at)
                .bind(&cursor.id);
        }
        Ok(statement.bind(limit).fetch_all(&self.pool).await?)
    }

    pub async fn messages_by_ids(&self, ids: &[String]) -> Result<Vec<MailSummary>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id IN ({placeholders}) ORDER BY received_at");
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
        Ok(sqlx::query_as::<_, MailSummary>("SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND mailbox = ? AND uid = ?")
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
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE classification_source IS NULL AND content_state = 'complete' ORDER BY received_at DESC";
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
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE classification_source IS NULL AND content_state = 'complete' ORDER BY received_at DESC, id DESC LIMIT ?";
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

    /// Returns current physical messages from one exact sender for a durable
    /// bulk mailbox action. Pending action memberships are omitted so a retry
    /// cannot enqueue a second mutation for a row already hidden locally.
    pub async fn messages_from_sender(
        &self,
        account_id: AccountId,
        sender_address: &str,
    ) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE account_id = ? AND from_address = ? COLLATE NOCASE AND mailbox NOT LIKE '__pending_action__:%' ORDER BY received_at DESC, id DESC";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(account_id.to_string())
            .bind(sender_address.trim())
            .fetch_all(&self.pool)
            .await?)
    }

    /// Messages eligible for an explicitly requested model reclassification.
    /// User-selected categories are deliberately excluded.
    pub async fn messages_for_model_reclassification(&self) -> Result<Vec<MailSummary>> {
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE content_state = 'complete' AND (classification_source IS NULL OR classification_source = 'model') ORDER BY received_at DESC";
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

    /// Applies Gmail/provider-derived classification evidence only while the
    /// Account used for that fetch is still the persisted provider identity.
    /// A false result discards old-server labels after an account update.
    pub async fn update_classification_signals_if_account_current(
        &self,
        receipt: &AccountProviderReceipt,
        updates: &[(String, String)],
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if ensure_account_provider_receipt_current_in_transaction(&mut tx, receipt)
            .await
            .is_err()
        {
            tx.rollback().await?;
            return Ok(false);
        }
        for (id, signals) in updates {
            sqlx::query("UPDATE messages SET classification_source = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_source END, classification_confidence = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_confidence END, classification_signals = ? WHERE id = ? AND account_id = ?")
                .bind(signals).bind(signals).bind(signals).bind(id).bind(receipt.account_id.to_string())
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Applies provider-derived classification evidence using the same
    /// namespace and local-mutation receipt that fenced the FETCH command.
    ///
    /// Category labels are metadata, but they still originate from a remote
    /// mailbox.  Updating by a local message id after a UIDVALIDITY rollover
    /// could otherwise attach evidence from an old server or old locator to a
    /// recycled row.  Each update is keyed by UID and only applies while that
    /// UID's version remains the one captured before the provider request.
    /// A `false` result means the account or mailbox identity changed, so the
    /// entire response must be discarded.  A newer local mutation for one UID
    /// merely leaves that individual row untouched.
    pub async fn update_classification_signals_with_provider_receipt(
        &self,
        receipt: &ProviderWriteReceipt,
        updates: &[(u32, String)],
    ) -> Result<bool> {
        let account_id = receipt.account_id.to_string();
        let captured_uids: HashSet<i64> = receipt
            .expected_flags
            .iter()
            .map(|expected| expected.uid)
            .collect();
        if updates
            .iter()
            .any(|(uid, _)| *uid == 0 || !captured_uids.contains(&i64::from(*uid)))
        {
            return Err(anyhow!(
                "classification update is not covered by its provider write receipt"
            ));
        }
        let expected_versions: HashMap<i64, i64> = receipt
            .local_mutation_versions
            .iter()
            .map(|version| (version.uid, version.version))
            .collect();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state AS catalogue WHERE catalogue.account_id = ? AND catalogue.mailbox = ? AND catalogue.remote_name = ? AND catalogue.uid_validity = ? AND catalogue.provider_config_generation = ? AND catalogue.provider_config_fingerprint = ? AND EXISTS (SELECT 1 FROM accounts AS account WHERE account.id = catalogue.account_id AND account.config_generation = ? AND account.config_fingerprint = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_generations AS replacement WHERE replacement.account_id = catalogue.account_id AND replacement.mailbox = catalogue.mailbox AND replacement.uid_validity != catalogue.uid_validity))")
            .bind(&account_id)
            .bind(&receipt.mailbox)
            .bind(&receipt.remote_name)
            .bind(i64::from(receipt.uid_validity))
            .bind(receipt.account_config_generation)
            .bind(&receipt.account_config_fingerprint)
            .bind(receipt.account_config_generation)
            .bind(&receipt.account_config_fingerprint)
            .fetch_one(&mut *tx)
            .await?;
        if !current {
            tx.rollback().await?;
            return Ok(false);
        }
        for (uid, signals) in updates {
            let expected_version = expected_versions
                .get(&i64::from(*uid))
                .copied()
                .unwrap_or(0);
            sqlx::query("UPDATE messages SET classification_source = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_source END, classification_confidence = CASE WHEN classification_source = 'model' AND classification_signals != ? THEN NULL ELSE classification_confidence END, classification_signals = ? WHERE account_id = ? AND mailbox = ? AND uid = ? AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_fences AS fence WHERE fence.account_id = messages.account_id AND fence.mailbox = messages.mailbox AND fence.uid = messages.uid AND (fence.uid_validity IS NULL OR fence.uid_validity = ?)) AND COALESCE((SELECT version.version FROM mailbox_mutation_versions AS version WHERE version.account_id = messages.account_id AND version.mailbox = messages.mailbox AND version.uid = messages.uid AND version.uid_validity = ?), 0) = ?")
                .bind(signals)
                .bind(signals)
                .bind(signals)
                .bind(&account_id)
                .bind(&receipt.mailbox)
                .bind(i64::from(*uid))
                .bind(i64::from(receipt.uid_validity))
                .bind(i64::from(receipt.uid_validity))
                .bind(expected_version)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
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
    PreserveLocal,
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
    uid_validity: i64,
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
            let mut message: MailSummary = serde_json::from_str(message_json)
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
            message.id = replacement_message_id(account_id, mailbox, *uid, uid_validity)?;
            for attachment in &mut message.attachments {
                attachment.attachment.message_id = message.id.clone();
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
    let local_mutation_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mailbox_mutation_fences AS fence JOIN mailbox_catalog_state AS catalogue ON catalogue.account_id = fence.account_id AND catalogue.mailbox = fence.mailbox WHERE fence.account_id = ? AND fence.mailbox = ? AND fence.uid = ? AND (fence.uid_validity IS NULL OR fence.uid_validity = catalogue.uid_validity))",
    )
    .bind(&message.account_id)
    .bind(&message.mailbox)
    .bind(message.uid)
    .fetch_one(&mut **tx)
    .await?;
    let (requested_provider_authority, expected_flags, compare_allowed) = match flag_policy {
        FlagUpdatePolicy::ProviderAuthoritative => (true, None, true),
        FlagUpdatePolicy::CompareAndSwap(expected_flags) => (false, expected_flags, true),
        FlagUpdatePolicy::PreserveLocal => (false, None, false),
    };
    // Historical, realtime, and delayed catalogue writers all share this
    // path. A pending optimistic operation keeps its local flags until its
    // journal outcome is reconciled, regardless of which writer arrives.
    let provider_authoritative = requested_provider_authority && !local_mutation_pending;
    let (expected_read, expected_flagged) = expected_flags.unwrap_or_default();
    sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET message_id=excluded.message_id, in_reply_to=excluded.in_reply_to, reference_ids=excluded.reference_ids, threading_scanned=1, recipient_headers_scanned=1, subject=excluded.subject, from_name=excluded.from_name, from_address=excluded.from_address, to_addresses=excluded.to_addresses, cc_addresses=excluded.cc_addresses, bcc_addresses=excluded.bcc_addresses, reply_to_addresses=excluded.reply_to_addresses, received_at=excluded.received_at, snippet=CASE WHEN excluded.content_state = 'complete' THEN excluded.snippet ELSE messages.snippet END, body_text=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_text ELSE messages.body_text END, body_html=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_html ELSE messages.body_html END, content_state=CASE WHEN messages.content_state = 'complete' THEN messages.content_state ELSE excluded.content_state END, unsubscribe_kind=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_kind ELSE messages.unsubscribe_kind END, unsubscribe_url=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_url ELSE messages.unsubscribe_url END, unsubscribe_scanned=CASE WHEN excluded.content_state = 'complete' THEN 1 ELSE messages.unsubscribe_scanned END, is_read=CASE WHEN ? OR (? AND messages.is_read = ? AND messages.is_flagged = ?) THEN excluded.is_read ELSE messages.is_read END, is_flagged=CASE WHEN ? OR (? AND messages.is_read = ? AND messages.is_flagged = ?) THEN excluded.is_flagged ELSE messages.is_flagged END, has_attachments=CASE WHEN excluded.content_state = 'complete' THEN excluded.has_attachments ELSE messages.has_attachments END, classification_confidence=CASE WHEN messages.classification_source = 'model' AND (messages.from_name IS NOT excluded.from_name OR messages.from_address != excluded.from_address OR messages.subject != excluded.subject OR messages.classification_signals != excluded.classification_signals OR (excluded.content_state = 'complete' AND (messages.snippet != excluded.snippet OR messages.body_text != excluded.body_text))) THEN NULL ELSE messages.classification_confidence END, classification_source=CASE WHEN messages.classification_source = 'model' AND (messages.from_name IS NOT excluded.from_name OR messages.from_address != excluded.from_address OR messages.subject != excluded.subject OR messages.classification_signals != excluded.classification_signals OR (excluded.content_state = 'complete' AND (messages.snippet != excluded.snippet OR messages.body_text != excluded.body_text))) THEN NULL ELSE messages.classification_source END, classification_signals=excluded.classification_signals")
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
        .bind(message.is_flagged).bind(message.has_attachments)
        .bind(&message.category).bind(message.classification_confidence)
        .bind(&message.classification_source).bind(&message.classification_signals)
        .bind(provider_authoritative)
        .bind(compare_allowed && expected_flags.is_some())
        .bind(expected_read)
        .bind(expected_flagged)
        .bind(provider_authoritative)
        .bind(compare_allowed && expected_flags.is_some())
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
    let effective_is_flagged = if !provider_authoritative {
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

fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
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
    error
        .chain()
        .any(|cause| cause.to_string().contains("duplicate column name"))
}

async fn save_account_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account: &Account,
) -> Result<()> {
    let deleted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
    )
    .bind(account.id.to_string())
    .fetch_one(&mut **tx)
    .await?;
    if deleted {
        return Err(anyhow!("account was removed"));
    }
    let fingerprint = account_provider_config_fingerprint(account)?;
    let prior: Option<(i64, String)> =
        sqlx::query_as("SELECT config_generation, config_fingerprint FROM accounts WHERE id = ?")
            .bind(account.id.to_string())
            .fetch_optional(&mut **tx)
            .await?;
    let configuration_changed = prior
        .as_ref()
        .is_some_and(|(_, prior_fingerprint)| prior_fingerprint != &fingerprint);
    let generation = match prior.as_ref() {
        Some((generation, _)) if configuration_changed => generation + 1,
        Some((generation, _)) => *generation,
        None => 1,
    };
    sqlx::query("INSERT INTO accounts(id, email, data, config_generation, config_fingerprint, created_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET email=excluded.email, data=excluded.data, config_generation=excluded.config_generation, config_fingerprint=excluded.config_fingerprint")
        .bind(account.id.to_string())
        .bind(&account.email)
        .bind(serde_json::to_string(account)?)
        .bind(generation)
        .bind(fingerprint)
        .bind(account.created_at)
        .execute(&mut **tx)
        .await?;
    if configuration_changed {
        // Keep retained catalogue rows and their cached bodies readable, but
        // make their remote locators unusable until a fetch from the new
        // account configuration republishes the mailbox identity.
        sqlx::query("UPDATE mailbox_catalog_state SET provider_config_generation = NULL, provider_config_fingerprint = NULL, updated_at = ? WHERE account_id = ?")
            .bind(Utc::now())
            .bind(account.id.to_string())
            .execute(&mut **tx)
            .await?;
        invalidate_remote_operations_for_account_configuration_change(tx, account.id).await?;
    }
    Ok(())
}

/// A provider configuration update changes the meaning of every IMAP
/// locator, even if a replacement server reuses the same UIDVALIDITY.  Mark
/// remote operations as unresolved inside the account-update transaction so
/// an old worker cannot claim or complete them after the new settings become
/// visible.  Mailbox actions are first restored from their durable backup:
/// leaving their source hidden in a synthetic pending mailbox would make an
/// unresolved intent disappear from the user's catalogue.
async fn invalidate_remote_operations_for_account_configuration_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: AccountId,
) -> Result<()> {
    let account_id_text = account_id.to_string();
    let backups: Vec<(String, String, String, i64, String)> = sqlx::query_as(
        "SELECT backup.operation_id, backup.message_id, backup.original_mailbox, backup.original_uid, backup.message_json FROM operation_message_backups AS backup JOIN operation_journal AS operation ON operation.operation_id = backup.operation_id WHERE operation.account_id = ? AND operation.kind = 'mailbox_action' AND operation.state NOT IN ('accepted', 'completed', 'rejected', 'permanent_failed', 'uncertain')",
    )
    .bind(&account_id_text)
    .fetch_all(&mut **tx)
    .await?;
    for (operation_id, message_id, mailbox, uid, message_json) in backups {
        let pending_mailbox = format!("__pending_action__:{operation_id}");
        let restored = sqlx::query(
            "UPDATE messages SET mailbox = ?, uid = ? WHERE id = ? AND account_id = ? AND mailbox = ?",
        )
        .bind(&mailbox)
        .bind(uid)
        .bind(&message_id)
        .bind(&account_id_text)
        .bind(&pending_mailbox)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if restored == 0 {
            let message: MailSummary = serde_json::from_str(&message_json)
                .context("decode mailbox-action backup during account update")?;
            persist_message(tx, &message).await?;
        }
        sqlx::query(
            "DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ? AND uid = ?",
        )
        .bind(&account_id_text)
        .bind(&mailbox)
        .bind(uid)
        .execute(&mut **tx)
        .await?;
    }
    // Sent-copy reconciliation also issues IMAP commands.  Do not let a
    // previously SMTP-accepted row append or search in a newly configured
    // account; retain its independent marker so recovery can tell delivery
    // uncertainty from the Sent-copy uncertainty.
    sqlx::query("UPDATE operation_journal SET state = 'uncertain', outcome = CASE WHEN smtp_accepted_at IS NOT NULL THEN 'smtp_accepted_account_configuration_changed' ELSE 'account_configuration_changed' END, error = 'account provider configuration changed before remote outcome', next_retry_at = NULL, claim_owner = NULL, claimed_at = NULL, claimed_from_state = NULL, updated_at = ? WHERE account_id = ? AND (state NOT IN ('accepted', 'completed', 'rejected', 'permanent_failed', 'uncertain') OR (kind = 'smtp_submission' AND state = 'accepted'))")
        .bind(Utc::now())
        .bind(&account_id_text)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM mailbox_mutation_fences WHERE operation_id IN (SELECT operation_id FROM operation_journal WHERE account_id = ? AND state = 'uncertain' AND outcome IN ('account_configuration_changed', 'smtp_accepted_account_configuration_changed'))")
        .bind(&account_id_text)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Canonical provider connection settings. Deliberately excludes mutable
/// OAuth expiry and UI-only fields, but includes every value that can select a
/// different IMAP namespace or authenticate as a different mailbox owner.
fn account_provider_config_fingerprint(account: &Account) -> Result<String> {
    let auth = match &account.auth {
        AccountAuth::Password { username } => serde_json::json!({
            "type": "password",
            "username": username.trim(),
        }),
        AccountAuth::OAuth2 {
            username, provider, ..
        } => serde_json::json!({
            "type": "oauth2",
            "username": username.trim(),
            "provider": provider.trim().to_lowercase(),
        }),
    };
    serde_json::to_string(&serde_json::json!({
        "account_id": account.id,
        "provider_id": account.provider_id.trim().to_lowercase(),
        "auth": auth,
        "imap_host": account.imap_host.trim().to_lowercase(),
        "imap_port": account.imap_port,
        "imap_security": account.imap_security,
        "archive_mailbox": account.archive_mailbox.trim(),
        "spam_mailbox": account.spam_mailbox.trim(),
    }))
    .context("could not serialize account provider configuration")
}

fn folder_sync_state_matches_provider_receipt(
    state: &FolderSyncState,
    receipt: &AccountProviderReceipt,
) -> bool {
    state.account_id == receipt.account_id.to_string()
        && state.account_config_generation == Some(receipt.config_generation)
        && state.account_config_fingerprint.as_deref() == Some(receipt.config_fingerprint.as_str())
}

async fn ensure_account_provider_receipt_current_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    receipt: &AccountProviderReceipt,
) -> Result<()> {
    let account_id = receipt.account_id.to_string();
    let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND config_generation = ? AND config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
        .bind(&account_id)
        .bind(receipt.config_generation)
        .bind(&receipt.config_fingerprint)
        .bind(&account_id)
        .fetch_one(&mut **tx)
        .await?;
    if current {
        Ok(())
    } else {
        Err(anyhow!(
            "account provider configuration changed during sync"
        ))
    }
}

async fn ensure_stored_provider_receipt_current_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    config_generation: Option<i64>,
    config_fingerprint: Option<&str>,
) -> Result<()> {
    match (config_generation, config_fingerprint) {
        (None, None) => Ok(()),
        (Some(generation), Some(fingerprint)) => {
            let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND config_generation = ? AND config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))")
                .bind(account_id)
                .bind(generation)
                .bind(fingerprint)
                .bind(account_id)
                .fetch_one(&mut **tx)
                .await?;
            if current {
                Ok(())
            } else {
                Err(anyhow!(
                    "account provider configuration changed during sync"
                ))
            }
        }
        _ => Err(anyhow!(
            "folder or mailbox snapshot has an invalid provider receipt"
        )),
    }
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
    // UUID v4 is used for accounts; deriving a stable ID avoids duplicates during resync.
    format!("{}:{}:{}", account_id, mailbox.replace(':', "_"), uid)
}

/// A UID is only unique inside one UIDVALIDITY namespace. Normal incremental
/// writes retain the long-standing ID; a replacement namespace gets this
/// canonical ID so a stale UI/content fetch receipt cannot resolve a recycled
/// UID to an unrelated message.
pub fn uidvalidity_message_id(
    account_id: AccountId,
    mailbox: &str,
    uid: u32,
    uid_validity: u32,
) -> String {
    format!(
        "{}:uv:{uid_validity}",
        stable_message_id(account_id, mailbox, uid)
    )
}

fn replacement_message_id(
    account_id: &str,
    mailbox: &str,
    uid: i64,
    uid_validity: i64,
) -> Result<String> {
    Ok(uidvalidity_message_id(
        AccountId::parse_str(account_id)?,
        mailbox,
        u32::try_from(uid).context("replacement UID is invalid")?,
        u32::try_from(uid_validity).context("replacement UIDVALIDITY is invalid")?,
    ))
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
    async fn owned_account_update_rolls_back_credentials_and_rebuild_on_failure_or_lost_ownership()
    {
        let store = Store::in_memory().await.unwrap();
        let original = legacy_oauth_account(uuid::Uuid::new_v4());
        let secret_name = format!(
            "dev.dakia.mail:{}:{}",
            original.id,
            original.auth.username()
        );
        store
            .save_account_with_secret(&original, &secret_name, "original-secret")
            .await
            .unwrap();
        let gate = store
            .acquire_account_removal_gate(original.id)
            .await
            .unwrap()
            .unwrap();
        let mut changed = original.clone();
        changed.display_name = "Updated account".into();
        let rebuild = MailRebuildJob {
            account_id: original.id,
            phase: "downloading".into(),
            completed: 0,
            total: None,
            reset_before_sync: true,
        };
        sqlx::query("CREATE TRIGGER reject_rebuild BEFORE INSERT ON mail_rebuild_jobs BEGIN SELECT RAISE(ABORT, 'forced rebuild failure'); END")
            .execute(&store.pool).await.unwrap();
        assert!(gate
            .save_account_with_secret_and_rebuild(
                &changed,
                &secret_name,
                Some("replacement-secret"),
                Some(&rebuild),
                None
            )
            .await
            .is_err());
        assert_eq!(
            store
                .account(original.id)
                .await
                .unwrap()
                .unwrap()
                .display_name,
            original.display_name
        );
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("original-secret")
        );
        assert!(store.mail_rebuild_jobs().await.unwrap().is_empty());
        sqlx::query("DROP TRIGGER reject_rebuild")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE account_removal_gates SET owner = 'replacement-owner' WHERE account_id = ?",
        )
        .bind(original.id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(gate
            .save_account_with_secret_and_rebuild(
                &changed,
                &secret_name,
                Some("replacement-secret"),
                Some(&rebuild),
                None
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("ownership expired"));
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("original-secret")
        );
        assert!(store.mail_rebuild_jobs().await.unwrap().is_empty());
        assert!(gate.delete_account_and_secret(&secret_name).await.is_err());
        assert!(store.account(original.id).await.unwrap().is_some());
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("original-secret")
        );
    }

    #[tokio::test]
    async fn new_account_credentials_and_initial_sync_commit_together_and_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("account.db");
        let store = Store::open(&path).await.unwrap();
        let account = account_with_id(uuid::Uuid::new_v4(), "new@example.test");
        let name = format!("dev.dakia.mail:{}:{}", account.id, account.auth.username());
        sqlx::query("CREATE TRIGGER reject_initial_job BEFORE INSERT ON sync_runs BEGIN SELECT RAISE(ABORT, 'forced initial job failure'); END").execute(&store.pool).await.unwrap();
        assert!(store
            .create_account_with_secret_and_initial_sync(&account, &name, "fictional-secret")
            .await
            .is_err());
        assert!(store.account(account.id).await.unwrap().is_none());
        assert!(store.secret(&name).await.unwrap().is_none());
        assert!(store.sync_runs().await.unwrap().is_empty());
        sqlx::query("DROP TRIGGER reject_initial_job")
            .execute(&store.pool)
            .await
            .unwrap();
        let run = store
            .create_account_with_secret_and_initial_sync(&account, &name, "fictional-secret")
            .await
            .unwrap();
        let mut duplicate = account.clone();
        duplicate.id = uuid::Uuid::new_v4();
        duplicate.email = " NEW@example.test ".into();
        assert!(store
            .create_account_with_secret_and_initial_sync(
                &duplicate,
                "duplicate-secret",
                "never-persisted"
            )
            .await
            .is_err());
        assert!(store.secret("duplicate-secret").await.unwrap().is_none());
        drop(store);
        let reopened = Store::open(&path).await.unwrap();
        assert!(reopened.account(account.id).await.unwrap().is_some());
        assert_eq!(
            reopened.secret(&name).await.unwrap().as_deref(),
            Some("fictional-secret")
        );
        let restored = reopened.sync_run(account.id).await.unwrap().unwrap();
        assert_eq!(restored.run_id, run.run_id);
        assert_eq!(restored.stage, "initial_inbox");
        assert_eq!(restored.outcome, "running");
        assert!(!restored.inbox_ready);
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
    async fn recent_body_cache_candidates_preserve_distinct_local_messages_with_the_same_rfc_id() {
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
                duplicate_old.clone(),
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
                duplicate_old.id.as_str(),
                flagged_missing.id.as_str(),
                sent.id.as_str(),
                archive.id.as_str(),
                boundary.id.as_str(),
            ]
        );
        for (offset, expected) in [
            (
                0,
                vec![duplicate_new.id.as_str(), duplicate_old.id.as_str()],
            ),
            (2, vec![flagged_missing.id.as_str(), sent.id.as_str()]),
            (4, vec![archive.id.as_str(), boundary.id.as_str()]),
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
    async fn foreground_reader_takes_over_background_content_without_waiting_for_its_lease() {
        let store = Store::in_memory().await.unwrap();
        let message = message("Read while warming", "preview");
        let id = message.id.clone();
        store.upsert_messages(&[message]).await.unwrap();
        let background = store
            .acquire_background_message_content_fetch(&id)
            .await
            .unwrap()
            .unwrap();
        let reader = match store
            .acquire_message_content_fetch_outcome(&id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(reader) => reader,
            _ => panic!("opening mail must not wait for low-priority warming"),
        };
        background.release().await.unwrap();
        assert!(
            store
                .acquire_background_message_content_fetch(&id)
                .await
                .unwrap()
                .is_none(),
            "the late warmer must not release the reader's replacement lease"
        );
        assert!(matches!(
            store
                .acquire_message_content_fetch_outcome(&id)
                .await
                .unwrap(),
            MessageContentFetchAcquire::Busy
        ));
        reader.release().await.unwrap();
        store
            .acquire_background_message_content_fetch(&id)
            .await
            .unwrap()
            .unwrap()
            .release()
            .await
            .unwrap();
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

        let destination_id = store
            .message_by_locator(account_id, "Archive", 3)
            .await
            .unwrap()
            .unwrap()
            .id;
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
        let destination_id = store
            .message_by_locator(account_id, "Archive", 3)
            .await
            .unwrap()
            .expect("move must retain the message under a fresh local identity")
            .id;
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
    async fn sync_content_status_tracks_live_claims_after_header_completion_and_expiry() {
        let store = Store::in_memory().await.unwrap();
        let account = account_with_id(uuid::Uuid::new_v4(), "loading@example.test");
        let other = account_with_id(uuid::Uuid::new_v4(), "other@example.test");
        store.save_account(&account).await.unwrap();
        store.save_account(&other).await.unwrap();
        let run = store.create_sync_run(account.id).await.unwrap();
        store.create_sync_run(other.id).await.unwrap();
        store
            .update_sync_run(
                &run.run_id,
                &SyncRunUpdate {
                    outcome: Some("completed"),
                    ..SyncRunUpdate::default()
                },
            )
            .await
            .unwrap();
        let mut row = message("Loading content", "preview");
        row.account_id = account.id.to_string();
        store.upsert_messages(&[row.clone()]).await.unwrap();
        assert!(
            !store
                .sync_run(account.id)
                .await
                .unwrap()
                .unwrap()
                .content_loading
        );
        let claim = store
            .acquire_background_message_content_fetch(&row.id)
            .await
            .unwrap()
            .unwrap();
        let loaded = store.sync_run(account.id).await.unwrap().unwrap();
        assert_eq!(loaded.outcome, "completed");
        assert!(loaded.content_loading);
        let statuses = store.sync_runs().await.unwrap();
        assert!(
            statuses
                .iter()
                .find(|status| status.account_id == account.id)
                .unwrap()
                .content_loading
        );
        assert!(
            !statuses
                .iter()
                .find(|status| status.account_id == other.id)
                .unwrap()
                .content_loading
        );
        claim.release().await.unwrap();
        assert!(
            !store
                .sync_run(account.id)
                .await
                .unwrap()
                .unwrap()
                .content_loading
        );
        let stale = store
            .acquire_background_message_content_fetch(&row.id)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE message_content_fetches SET claimed_at = ? WHERE message_id = ?")
            .bind(Utc::now() - chrono::Duration::seconds(MESSAGE_CONTENT_FETCH_LEASE_SECONDS + 1))
            .bind(&row.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            !store
                .sync_run(account.id)
                .await
                .unwrap()
                .unwrap()
                .content_loading
        );
        assert!(!store
            .sync_runs()
            .await
            .unwrap()
            .iter()
            .any(|status| status.content_loading));
        stale.release().await.unwrap();
    }

    #[tokio::test]
    async fn old_account_content_cannot_replace_or_evict_newer_cached_body() {
        let store = Store::in_memory().await.unwrap();
        let account = account_with_id(uuid::Uuid::new_v4(), "reader@example.test");
        store.save_account(&account).await.unwrap();
        let mut row = message("Cached content", "preview");
        row.account_id = account.id.to_string();
        store.upsert_messages(&[row.clone()]).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 1, true)
            .await
            .unwrap();
        let old_identity = store
            .capture_message_remote_identity(&row.id)
            .await
            .unwrap()
            .unwrap();
        let mut changed = account.clone();
        changed.imap_host = "replacement.example.test".into();
        store.save_account(&changed).await.unwrap();
        let current_receipt = store
            .capture_account_provider_receipt(&changed)
            .await
            .unwrap()
            .unwrap();
        let mut forged_current_config = old_identity.clone();
        forged_current_config.account_config_generation = current_receipt.config_generation;
        forged_current_config.account_config_fingerprint = current_receipt.config_fingerprint;
        assert!(store
            .capture_message_remote_identity(&row.id)
            .await
            .unwrap()
            .is_none());
        assert!(!store
            .cache_message_content_if_current(
                &forged_current_config,
                false,
                cached_content("forged new-account receipt over old locator")
            )
            .await
            .unwrap());
        assert!(!store
            .set_message_content_state_if_current(&forged_current_config, "complete")
            .await
            .unwrap());
        assert!(!store
            .update_message_attachment_state_if_current(&forged_current_config, true)
            .await
            .unwrap());
        let state = store
            .begin_folder_sync_for_account(&changed, "INBOX", "INBOX", 77, Some(1))
            .await
            .unwrap();
        let revision = store
            .stage_folder_discovery_page(
                changed.id,
                "INBOX",
                &state.generation,
                state.revision as u64,
                &[u32::try_from(row.uid).unwrap()],
                None,
                true,
            )
            .await
            .unwrap();
        store
            .commit_folder_header_batch_for_account(
                &changed,
                "INBOX",
                &state.generation,
                revision,
                &[row.clone()],
                None,
                true,
            )
            .await
            .unwrap();
        let current_identity = store
            .capture_message_remote_identity(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .cache_message_content_if_current(
                &current_identity,
                false,
                cached_content("new server body")
            )
            .await
            .unwrap());
        assert!(!store
            .cache_message_content_if_current(
                &old_identity,
                false,
                cached_content("stale old server body")
            )
            .await
            .unwrap());
        assert!(!store
            .cache_message_content_with_budget(
                &row.id,
                false,
                cached_content(&"x".repeat(100)),
                20,
                Some(&old_identity)
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .cached_message_content(&row.id)
                .await
                .unwrap()
                .unwrap()
                .body_text,
            "new server body"
        );
        store.set_message_flagged(&row.id, true).await.unwrap();
        assert!(store
            .cache_starred_message_content_if_current(
                &current_identity,
                cached_content("new starred body")
            )
            .await
            .unwrap());
        assert!(!store
            .cache_starred_message_content_if_current(
                &old_identity,
                cached_content("old starred body")
            )
            .await
            .unwrap());
        assert_eq!(
            store.starred_body(&row.id).await.unwrap().unwrap().0,
            "new starred body"
        );
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
            .cache_message_content_with_budget(&id, false, cached_content(&oversized), 20, None)
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
            .is_some());
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
                "unsorted",
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

        let full = store
            .search_conversation_page(&SearchQuery {
                account_ids: vec![account_id],
                limit: Some(500),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(full.conversations.len(), 500);
        assert!(full.next_cursor.is_some());
    }

    #[test]
    fn conversation_page_serializes_the_tauri_continuation_as_next_cursor() {
        let page = MailConversationPage {
            conversations: Vec::new(),
            next_cursor: Some(MailCursor {
                received_at: "2026-07-27T12:00:00Z".parse().unwrap(),
                id: "message-id".into(),
            }),
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
    async fn optimistic_mailbox_action_reconciles_the_operation_owned_pending_membership() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        assert!(store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 11)
            .await
            .unwrap());
        assert!(store
            .ensure_mailbox_catalog_identity(account_id, "Archive", "Archive", 12)
            .await
            .unwrap());
        let mut source =
            cache_candidate_message(account_id, "action-source", 41, "INBOX", Utc::now());
        source.id = uidvalidity_message_id(account_id, "INBOX", 41, 11);
        store
            .upsert_messages(std::slice::from_ref(&source))
            .await
            .unwrap();
        store
            .cache_message_content(&source.id, false, cached_content("source cache"))
            .await
            .unwrap();
        store
            .observe_gmail_message(
                account_id,
                &source.id,
                "gmail-action-source",
                &["\\Inbox".into()],
            )
            .await
            .unwrap();
        store
            .observe_message_dates(account_id, &source.id, Utc::now(), None)
            .await
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&source.id)
            .await
            .unwrap()
            .unwrap();

        let operation = store
            .enqueue_and_apply_mailbox_action_for_identity(
                &identity,
                crate::mail::MailboxAction::Archive,
                None,
            )
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 41)
            .await
            .unwrap()
            .is_none());
        let pending_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = ? AND mailbox = ?")
                .bind(&source.id)
                .bind(format!("__pending_action__:{}", operation.operation_id))
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(pending_count, 1);

        store
            .claim_operation_by_id(&operation.operation_id, "action-worker")
            .await
            .unwrap()
            .expect("queued action must be claimed");
        assert!(store
            .reconcile_and_complete_claimed_mailbox_action(
                &operation.operation_id,
                "action-worker",
                "Archive",
                Some(7),
            )
            .await
            .unwrap());

        let destination = store
            .message_by_locator(account_id, "Archive", 7)
            .await
            .unwrap()
            .expect("remote-confirmed move must become visible");
        assert_eq!(
            destination.id,
            uidvalidity_message_id(account_id, "Archive", 7, 12)
        );
        assert_eq!(
            store
                .gmail_message_labels(account_id, &destination.id)
                .await
                .unwrap(),
            Some(vec!["\\Inbox".into()])
        );
        let temporal_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_temporal_observations WHERE account_id = ? AND message_id = ?",
        )
        .bind(account_id.to_string())
        .bind(&destination.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(temporal_rows, 1);
        assert!(store
            .cached_message_content(&source.id)
            .await
            .unwrap()
            .is_none());
        let pending_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE mailbox LIKE '__pending_action__:%'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let backup_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM operation_message_backups WHERE operation_id = ?",
        )
        .bind(&operation.operation_id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(pending_rows, 0);
        assert_eq!(backup_rows, 0);
        assert_eq!(
            store
                .operation_journal_entry(&operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
    }

    #[tokio::test]
    async fn optimistic_mailbox_action_rollback_restores_the_exact_source_membership() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        assert!(store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 21)
            .await
            .unwrap());
        let mut source =
            cache_candidate_message(account_id, "rollback-source", 42, "INBOX", Utc::now());
        source.id = uidvalidity_message_id(account_id, "INBOX", 42, 21);
        store
            .upsert_messages(std::slice::from_ref(&source))
            .await
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&source.id)
            .await
            .unwrap()
            .unwrap();
        let operation = store
            .enqueue_and_apply_mailbox_action_for_identity(
                &identity,
                crate::mail::MailboxAction::Delete,
                None,
            )
            .await
            .unwrap();
        store
            .claim_operation_by_id(&operation.operation_id, "action-worker")
            .await
            .unwrap()
            .expect("queued action must be claimed");
        assert!(store
            .rollback_and_complete_claimed_mailbox_action(
                &operation.operation_id,
                "action-worker",
                "remote delete rejected",
            )
            .await
            .unwrap());

        let restored = store
            .message_by_locator(account_id, "INBOX", 42)
            .await
            .unwrap()
            .expect("rollback must restore the visible source row");
        assert_eq!(restored.id, source.id);
        let tombstones: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = 'INBOX' AND uid = 42",
        )
        .bind(account_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let backup_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM operation_message_backups WHERE operation_id = ?",
        )
        .bind(&operation.operation_id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(tombstones, 0);
        assert_eq!(backup_rows, 0);
        assert_eq!(
            store
                .operation_journal_entry(&operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "permanent_failed"
        );
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
        spam.mailbox = "Spam".into();
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
        assert_eq!(spam_view[0].messages[0].mailbox, "Spam");
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
        store.upsert_messages(&[message.clone()]).await.unwrap();
        let listed = store.attachments(&message.id).await.unwrap();
        assert!(listed.is_empty());
        let stored = store
            .messages_by_ids(&[message.id])
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(stored.body_text.is_empty());
        assert!(stored.body_html.is_none());
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
        let matches = store
            .search(&SearchQuery {
                text: "needle".into(),
                account_ids: vec![account_id],
                ..SearchQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);
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
    async fn gmail_membership_epoch_ignores_flags_but_tracks_mailbox_transitions() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let inbox =
            cache_candidate_message(account_id, "gmail-epoch-inbox", 1, "INBOX", Utc::now());
        store.upsert_messages(&[inbox.clone()]).await.unwrap();
        let before = store
            .capture_gmail_inbox_membership_epoch(account_id)
            .await
            .unwrap();
        store.set_message_read(&inbox.id, true).await.unwrap();
        assert_eq!(
            store
                .capture_gmail_inbox_membership_epoch(account_id)
                .await
                .unwrap()
                .epoch,
            before.epoch
        );
        sqlx::query("UPDATE messages SET mailbox = '__pending_action__:epoch-test' WHERE id = ?")
            .bind(&inbox.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store
                .capture_gmail_inbox_membership_epoch(account_id)
                .await
                .unwrap()
                .epoch
                > before.epoch
        );
    }

    #[tokio::test]
    async fn provider_receipt_rejects_old_endpoint_after_account_update() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut old_account = account_with_id(account_id, "source-fence@example.test");
        old_account.imap_host = "old.imap.example.test".into();
        store.save_account(&old_account).await.unwrap();
        assert!(store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 1)
            .await
            .unwrap());
        let receipt = store
            .capture_provider_write_receipt_for_account(&old_account, "INBOX", "INBOX", 1, &[7])
            .await
            .unwrap()
            .unwrap();

        let mut updated_account = old_account.clone();
        updated_account.imap_host = "new.imap.example.test".into();
        store.save_account(&updated_account).await.unwrap();
        let mut stale =
            cache_candidate_message(account_id, "old-host-message", 7, "INBOX", Utc::now());
        stale.id = "old-host-message".into();
        assert!(!store
            .commit_provider_messages_with_receipt(&receipt, &[stale])
            .await
            .unwrap());
        assert!(store
            .message_by_locator(account_id, "INBOX", 7)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn smtp_submissions_queue_by_account_without_reusing_another_draft() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;

        let first = store
            .enqueue_smtp_submission_and_claim(account_id, r#"{"draft":"first"}"#, "cli-first")
            .await
            .unwrap();
        assert_eq!(first.state, "submitting");
        assert_eq!(first.payload_json, r#"{"draft":"first"}"#);

        let second = store
            .enqueue_smtp_submission_and_claim(account_id, r#"{"draft":"second"}"#, "cli-second")
            .await
            .unwrap();
        assert_ne!(second.operation_id, first.operation_id);
        assert_eq!(second.state, "queued");
        assert_eq!(second.payload_json, r#"{"draft":"second"}"#);
        assert!(store
            .claim_operation_by_id(&second.operation_id, "cli-second")
            .await
            .unwrap()
            .is_none());

        store
            .complete_claimed_operation(
                &first.operation_id,
                "cli-first",
                "accepted",
                Some("provider_sent_reconciliation"),
                None,
                None,
            )
            .await
            .unwrap();
        let accepted = store
            .operation_journal_entry(&first.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(accepted.smtp_accepted_at.is_some());

        let second_claim = store
            .claim_operation_by_id(&second.operation_id, "cli-second")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_claim.operation_id, second.operation_id);
        assert_eq!(second_claim.payload_json, r#"{"draft":"second"}"#);

        let sent_claim = store
            .claim_provider_sent_operation(&first.operation_id, "sent-copy")
            .await
            .unwrap()
            .unwrap();
        store
            .complete_claimed_operation(
                &sent_claim.operation_id,
                "sent-copy",
                "retry",
                Some("sent_copy_retry_scheduled"),
                Some("temporary append failure"),
                Some(Utc::now() + chrono::Duration::minutes(1)),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .operation_journal_entry(&first.operation_id)
                .await
                .unwrap()
                .unwrap()
                .smtp_accepted_at,
            accepted.smtp_accepted_at
        );
    }

    #[tokio::test]
    async fn gmail_epoch_batch_applies_together_and_rejects_a_stale_fetch() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let first =
            cache_candidate_message(account_id, "gmail-batch-one", 1, "Archive", Utc::now());
        let second =
            cache_candidate_message(account_id, "gmail-batch-two", 2, "Archive", Utc::now());
        store
            .upsert_messages(&[first.clone(), second.clone()])
            .await
            .unwrap();

        let receipt = store
            .capture_gmail_inbox_membership_epoch(account_id)
            .await
            .unwrap();
        assert!(store
            .observe_gmail_messages_with_epoch(
                &receipt,
                &[
                    GmailMessageObservation {
                        local_message_id: first.id.clone(),
                        gmail_message_id: "1001".into(),
                        labels: vec!["\\Inbox".into()],
                    },
                    GmailMessageObservation {
                        local_message_id: second.id.clone(),
                        gmail_message_id: "1002".into(),
                        labels: vec!["\\Inbox".into()],
                    },
                ],
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .gmail_message_labels(account_id, &second.id)
                .await
                .unwrap(),
            Some(vec!["\\Inbox".into()])
        );

        let stale = store
            .capture_gmail_inbox_membership_epoch(account_id)
            .await
            .unwrap();
        let inbox = cache_candidate_message(account_id, "gmail-new-inbox", 3, "INBOX", Utc::now());
        store.upsert_messages(&[inbox]).await.unwrap();
        assert!(!store
            .observe_gmail_messages_with_epoch(
                &stale,
                &[GmailMessageObservation {
                    local_message_id: first.id.clone(),
                    gmail_message_id: "1001".into(),
                    labels: vec!["\\All".into()],
                }],
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .gmail_message_labels(account_id, &first.id)
                .await
                .unwrap(),
            Some(vec!["\\Inbox".into()])
        );
    }

    #[tokio::test]
    async fn account_removal_gate_blocks_cross_process_claims_until_cancel_or_expiry() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let operation = store
            .enqueue_operation(
                account_id,
                "smtp_submission",
                OperationTarget {
                    mailbox: None,
                    uid: None,
                    uid_validity: None,
                    message_id: None,
                },
                "{}",
                None,
            )
            .await
            .unwrap();
        assert!(store.begin_account_removal(account_id).await.unwrap());
        assert!(store
            .claim_operation_by_id(&operation.operation_id, "separate-cli")
            .await
            .unwrap()
            .is_none());
        assert!(store.cancel_account_removal(account_id).await.unwrap());
        assert!(store
            .claim_operation_by_id(&operation.operation_id, "separate-cli")
            .await
            .unwrap()
            .is_some());
        assert!(store
            .renew_operation_claim(&operation.operation_id, "separate-cli")
            .await
            .unwrap());
        assert_eq!(
            store
                .mark_interrupted_operations_uncertain(account_id)
                .await
                .unwrap(),
            0
        );
        sqlx::query("UPDATE operation_journal SET claimed_at = ? WHERE operation_id = ?")
            .bind(Utc::now() - chrono::Duration::seconds(OPERATION_CLAIM_LEASE_SECONDS + 1))
            .bind(&operation.operation_id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .mark_interrupted_operations_uncertain(account_id)
                .await
                .unwrap(),
            1
        );

        assert!(store.begin_account_removal(account_id).await.unwrap());
        sqlx::query("UPDATE account_removal_gates SET expires_at = ? WHERE account_id = ?")
            .bind(Utc::now() - chrono::Duration::seconds(1))
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .recover_orphan_account_operation_gates()
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn delete_account_and_secret_commits_both_local_resources_together() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        store
            .set_secret("mail:remove-with-account", "token")
            .await
            .unwrap();
        store
            .delete_account_and_secret(account_id, "mail:remove-with-account")
            .await
            .unwrap();
        assert!(store.account(account_id).await.unwrap().is_none());
        assert!(store
            .secret("mail:remove-with-account")
            .await
            .unwrap()
            .is_none());
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
    async fn replacement_snapshot_finalization_uses_generation_scoped_ids() {
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

        store
            .finalize_mailbox_snapshot_with_replacements(
                account_id,
                "INBOX",
                &generation,
                &[replacement],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Replacement namespace"
        );
        assert_eq!(
            store
                .message("replacement-id-collision")
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Unrelated row"
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
                MailboxSnapshotIdentity::new("INBOX", 99, 0, Some(3), None),
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
    async fn header_failure_summary_separates_delayed_retries_from_user_action_and_content() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let due = Utc::now() + chrono::Duration::minutes(1);
        store
            .record_mailbox_sync_failure_with_retry(
                account_id,
                "INBOX",
                1,
                "headers",
                "network",
                "temporary",
                Some(due),
                false,
            )
            .await
            .unwrap();
        store
            .record_mailbox_sync_failure_with_retry(
                account_id,
                "INBOX",
                2,
                "headers",
                "malformed",
                "malformed header",
                None,
                true,
            )
            .await
            .unwrap();
        store
            .record_mailbox_sync_failure_with_retry(
                account_id,
                "INBOX",
                3,
                "preview",
                "network",
                "body unavailable",
                None,
                false,
            )
            .await
            .unwrap();
        let summary = store
            .mailbox_header_failure_summary(account_id, "INBOX")
            .await
            .unwrap();
        assert_eq!(summary.outstanding, 2);
        assert_eq!(summary.retryable, 1);
        assert_eq!(summary.user_action_required, 1);
        assert_eq!(summary.next_retry_at, Some(due));
        assert_eq!(
            store
                .mailbox_header_failure_summary(account_id, "Drafts")
                .await
                .unwrap()
                .outstanding,
            0
        );
        store
            .clear_mailbox_sync_failure(account_id, "INBOX", 1)
            .await
            .unwrap();
        let summary = store
            .mailbox_header_failure_summary(account_id, "INBOX")
            .await
            .unwrap();
        assert_eq!(summary.outstanding, 1);
        assert_eq!(summary.retryable, 0);
        assert!(summary.next_retry_at.is_none());
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

    #[tokio::test]
    async fn folder_generation_retries_reused_uids_and_rejects_stale_pages() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("old namespace", "old");
        old.account_id = account_id.to_string();
        old.uid = 7;
        store.upsert_catalog_messages(&[old]).await.unwrap();

        let first = store
            .begin_folder_sync(account_id, "INBOX", "INBOX", 10, Some(8))
            .await
            .unwrap();
        let revision = store
            .stage_folder_discovery_page(
                account_id,
                "INBOX",
                &first.generation,
                first.revision as u64,
                &[7],
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .folder_discovered_uids_needing_headers(account_id, "INBOX", 50)
                .await
                .unwrap(),
            vec![7],
            "an old UIDVALIDITY row cannot make a reused UID look fetched"
        );
        let mut replacement = message("replacement namespace", "new");
        replacement.account_id = account_id.to_string();
        replacement.uid = 7;
        store
            .commit_folder_header_batch(
                account_id,
                "INBOX",
                &first.generation,
                revision,
                &[replacement],
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "replacement namespace"
        );

        let replacement = store
            .begin_folder_sync(account_id, "INBOX", "INBOX", 11, Some(8))
            .await
            .unwrap();
        assert_ne!(replacement.generation, first.generation);
        let replacement_revision = store
            .stage_folder_discovery_page(
                account_id,
                "INBOX",
                &replacement.generation,
                replacement.revision as u64,
                &[7],
                None,
                true,
            )
            .await
            .unwrap();
        let mut recycled = message("must remain staged", "new UID namespace");
        recycled.account_id = account_id.to_string();
        recycled.uid = 7;
        assert!(store
            .commit_folder_header_batch(
                account_id,
                "INBOX",
                &replacement.generation,
                replacement_revision,
                &[recycled],
                None,
                true,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("UIDVALIDITY"));
        assert_eq!(
            store
                .message_by_locator(account_id, "INBOX", 7)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "replacement namespace",
            "recycled UIDs remain in staged replacement until atomic finalization"
        );
        assert!(store
            .stage_folder_discovery_page(
                account_id,
                "INBOX",
                &first.generation,
                first.revision as u64,
                &[7],
                None,
                true,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not active"));
    }

    #[tokio::test]
    async fn snapshot_boundary_and_local_mutation_fence_preserve_newer_local_rows() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut old = message("old", "old");
        old.account_id = account_id.to_string();
        old.uid = 200;
        let mut realtime = message("realtime", "new");
        realtime.account_id = account_id.to_string();
        realtime.uid = 201;
        store
            .upsert_catalog_messages(&[old, realtime])
            .await
            .unwrap();
        let generation = store
            .begin_mailbox_snapshot(
                account_id,
                "INBOX",
                MailboxSnapshotIdentity::new("INBOX", 1, 0, Some(201), None),
            )
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
            .message_by_locator(account_id, "INBOX", 201)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn sync_runs_keep_account_revisions_monotonic_across_terminal_runs() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;

        let first = store.create_sync_run(account_id).await.unwrap();
        assert_eq!(first.revision, 0);
        let completed = store
            .update_sync_run(
                &first.run_id,
                &SyncRunUpdate {
                    outcome: Some("completed"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let next = store.create_sync_run(account_id).await.unwrap();
        assert!(next.revision > completed.revision);
        assert_ne!(next.run_id, first.run_id);
        let visible = store.sync_runs().await.unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].run_id, next.run_id);
        assert!(store
            .update_sync_run(&first.run_id, &SyncRunUpdate::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn provider_receipt_keeps_toggle_away_and_back_newer_than_fetch() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut local = message("original", "receipt");
        local.account_id = account_id.to_string();
        local.uid = 7;
        local.is_read = false;
        store
            .upsert_catalog_messages(&[local.clone()])
            .await
            .unwrap();
        store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 9)
            .await
            .unwrap();
        let receipt = store
            .capture_provider_write_receipt(account_id, "INBOX", "INBOX", 9, &[7])
            .await
            .unwrap()
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&local.id)
            .await
            .unwrap()
            .unwrap();
        store
            .enqueue_and_apply_flag_mutation_for_identity(
                &identity,
                "message_read",
                Some(true),
                None,
                None,
            )
            .await
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&local.id)
            .await
            .unwrap()
            .unwrap();
        store
            .enqueue_and_apply_flag_mutation_for_identity(
                &identity,
                "message_read",
                Some(false),
                None,
                None,
            )
            .await
            .unwrap();
        let mut stale_provider = local;
        stale_provider.is_read = true;
        assert!(store
            .commit_provider_messages_with_receipt(&receipt, &[stale_provider])
            .await
            .unwrap());
        assert!(
            !store
                .message(&identity.message_id)
                .await
                .unwrap()
                .unwrap()
                .is_read
        );
    }

    #[tokio::test]
    async fn account_config_change_invalidates_old_mailbox_claims_and_restores_projection() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "operation-fence@example.test");
        store.save_account(&account).await.unwrap();
        let mut local = message("pending archive", "body");
        local.account_id = account_id.to_string();
        local.uid = 7;
        store
            .upsert_catalog_messages(&[local.clone()])
            .await
            .unwrap();
        store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 9)
            .await
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&local.id)
            .await
            .unwrap()
            .unwrap();
        let operation = store
            .enqueue_and_apply_mailbox_action_and_claim_for_identity(
                &identity,
                crate::mail::MailboxAction::Archive,
                None,
                "old-provider-worker",
            )
            .await
            .unwrap();
        assert!(store
            .message_by_locator(account_id, "INBOX", 7)
            .await
            .unwrap()
            .is_none());
        let active = store.claimed_account_operations(account_id).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].operation_id, operation.operation_id);
        assert_eq!(
            active[0].claim_owner.as_deref(),
            Some("old-provider-worker")
        );
        assert!(active[0].claimed_at.is_some());

        let mut changed = account.clone();
        changed.imap_host = "different-imap.example.test".into();
        store.save_account(&changed).await.unwrap();

        let invalidated = store
            .operation_journal_entry(&operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(invalidated.state, "uncertain");
        assert_eq!(
            invalidated.outcome.as_deref(),
            Some("account_configuration_changed")
        );
        assert!(store
            .claimed_account_operations(account_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .message_by_locator(account_id, "INBOX", 7)
            .await
            .unwrap()
            .is_some());
        assert!(
            store
                .capture_message_remote_identity(&local.id)
                .await
                .unwrap()
                .is_none(),
            "retained cached mail is not a new-host remote locator"
        );
        assert!(store
            .enqueue_and_apply_flag_mutation_for_identity(
                &identity,
                "message_read",
                Some(true),
                None,
                None,
            )
            .await
            .is_err());
        assert!(!store
            .reconcile_and_complete_claimed_mailbox_action::<MoveDestinationLocator>(
                &operation.operation_id,
                "old-provider-worker",
                "Archive",
                None,
            )
            .await
            .unwrap());
        assert!(store
            .claim_operation_by_id(&operation.operation_id, "old-provider-worker")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn invalidated_catalogue_locator_stays_blocked_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("catalogue-config-barrier.sqlite");
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "reopen-config-fence@example.test");
        let mut row = message("retained cached row", "cached body remains readable");
        row.account_id = account_id.to_string();
        row.uid = 7;
        {
            let store = Store::open(&database).await.unwrap();
            store.save_account(&account).await.unwrap();
            store.upsert_catalog_messages(&[row.clone()]).await.unwrap();
            store
                .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 9, 1, true)
                .await
                .unwrap();
            assert!(store
                .capture_message_remote_identity(&row.id)
                .await
                .unwrap()
                .is_some());
            let mut changed = account.clone();
            changed.imap_host = "replacement.example.test".into();
            store.save_account(&changed).await.unwrap();
        }
        let reopened = Store::open(&database).await.unwrap();
        assert_eq!(
            reopened.message(&row.id).await.unwrap().unwrap().subject,
            "retained cached row"
        );
        assert!(
            reopened
                .capture_message_remote_identity(&row.id)
                .await
                .unwrap()
                .is_none(),
            "reopen must not re-stamp an invalidated locator"
        );
    }

    #[tokio::test]
    async fn account_config_change_keeps_smtp_acceptance_distinct_from_sent_copy_uncertainty() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "smtp-config-fence@example.test");
        store.save_account(&account).await.unwrap();
        let operation = store
            .enqueue_smtp_submission_and_claim(account_id, r#"{"draft":"one"}"#, "smtp-worker")
            .await
            .unwrap();
        store
            .complete_claimed_operation(
                &operation.operation_id,
                "smtp-worker",
                "accepted",
                Some("provider_sent_reconciliation"),
                None,
                None,
            )
            .await
            .unwrap();

        let mut changed = account.clone();
        changed.imap_host = "different-imap.example.test".into();
        store.save_account(&changed).await.unwrap();

        let invalidated = store
            .operation_journal_entry(&operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(invalidated.state, "uncertain");
        assert_eq!(
            invalidated.outcome.as_deref(),
            Some("smtp_accepted_account_configuration_changed")
        );
        assert!(invalidated.smtp_accepted_at.is_some());
        assert!(store
            .claim_provider_sent_operation(&operation.operation_id, "new-worker")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn classification_signals_use_the_provider_receipt_namespace_and_version() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        save_test_account(&store, account_id).await;
        let mut local = message("category evidence", "receipt");
        local.account_id = account_id.to_string();
        local.uid = 7;
        local.classification_signals = "old evidence".into();
        store
            .upsert_catalog_messages(&[local.clone()])
            .await
            .unwrap();
        store
            .ensure_mailbox_catalog_identity(account_id, "INBOX", "INBOX", 9)
            .await
            .unwrap();

        let receipt = store
            .capture_provider_write_receipt(account_id, "INBOX", "INBOX", 9, &[7])
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .update_classification_signals_with_provider_receipt(
                &receipt,
                &[(7, "fresh provider evidence".into())],
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .message(&local.id)
                .await
                .unwrap()
                .unwrap()
                .classification_signals,
            "fresh provider evidence"
        );

        let receipt = store
            .capture_provider_write_receipt(account_id, "INBOX", "INBOX", 9, &[7])
            .await
            .unwrap()
            .unwrap();
        let identity = store
            .capture_message_remote_identity(&local.id)
            .await
            .unwrap()
            .unwrap();
        store
            .enqueue_and_apply_flag_mutation_for_identity(
                &identity,
                "message_read",
                Some(true),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(store
            .update_classification_signals_with_provider_receipt(
                &receipt,
                &[(7, "stale provider evidence".into())],
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .message(&local.id)
                .await
                .unwrap()
                .unwrap()
                .classification_signals,
            "fresh provider evidence",
            "a local mutation after FETCH fences a delayed provider label"
        );
    }

    #[tokio::test]
    async fn account_aware_folder_and_snapshot_generations_reject_changed_imap_endpoint() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "config-fence@example.test");
        store.save_account(&account).await.unwrap();

        let folder = store
            .begin_folder_sync_for_account(&account, "INBOX", "INBOX", 7, Some(2))
            .await
            .unwrap();
        let snapshot = store
            .begin_mailbox_snapshot_for_account(
                &account,
                "Archive",
                MailboxSnapshotIdentity::new("Archive", 9, 1, Some(2), None),
            )
            .await
            .unwrap();

        let mut changed_account = account.clone();
        changed_account.imap_host = "new-endpoint.example.test".into();
        store.save_account(&changed_account).await.unwrap();

        assert!(store
            .stage_folder_discovery_page(
                account_id,
                "INBOX",
                &folder.generation,
                folder.revision as u64,
                &[1],
                None,
                true,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("provider configuration changed"));
        assert!(store
            .stage_mailbox_snapshot_page(account_id, "Archive", &snapshot, &[(1, false, false)])
            .await
            .unwrap_err()
            .to_string()
            .contains("provider configuration changed"));
        assert!(store
            .finalize_mailbox_snapshot(account_id, "Archive", &snapshot)
            .await
            .unwrap_err()
            .to_string()
            .contains("provider configuration changed"));
        assert!(store
            .mailbox_catalog_state(account_id, "Archive")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn gmail_folder_header_batch_publishes_headers_and_labels_under_one_epoch_fence() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let account = account_with_id(account_id, "gmail-folder-batch@example.test");
        store.save_account(&account).await.unwrap();
        let account_receipt = store
            .capture_account_provider_receipt(&account)
            .await
            .unwrap()
            .unwrap();
        let folder = store
            .begin_folder_sync_with_provider_receipt(&account_receipt, "INBOX", "INBOX", 7, Some(1))
            .await
            .unwrap();
        let revision = store
            .stage_folder_discovery_page(
                account_id,
                "INBOX",
                &folder.generation,
                folder.revision as u64,
                &[1],
                None,
                true,
            )
            .await
            .unwrap();
        let epoch = store
            .capture_gmail_inbox_membership_epoch(account_id)
            .await
            .unwrap();
        let mut header = message("Gmail header", "initial");
        header.account_id = account_id.to_string();
        header.uid = 1;
        header.mailbox = "INBOX".into();
        let labels = vec!["\\Inbox".to_owned(), "\\Important".to_owned()];

        assert_eq!(
            store
                .commit_folder_header_batch_with_gmail_observations(
                    &account_receipt,
                    &epoch,
                    "INBOX",
                    &folder.generation,
                    revision,
                    &[header.clone()],
                    &[GmailProviderObservation {
                        uid: 1,
                        gmail_message_id: "9001".into(),
                        labels: labels.clone(),
                    }],
                    None,
                    true,
                )
                .await
                .unwrap(),
            revision + 1
        );
        assert!(store
            .message_by_locator(account_id, "INBOX", 1)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .gmail_message_labels(account_id, &header.id)
                .await
                .unwrap(),
            Some(labels)
        );

        let stale = store
            .begin_folder_sync_with_provider_receipt(
                &account_receipt,
                "Archive",
                "Archive",
                8,
                Some(1),
            )
            .await
            .unwrap();
        let stale_revision = store
            .stage_folder_discovery_page(
                account_id,
                "Archive",
                &stale.generation,
                stale.revision as u64,
                &[1],
                None,
                true,
            )
            .await
            .unwrap();
        let stale_epoch = store
            .capture_gmail_inbox_membership_epoch(account_id)
            .await
            .unwrap();
        let late_inbox = cache_candidate_message(account_id, "late-inbox", 2, "INBOX", Utc::now());
        store.upsert_messages(&[late_inbox]).await.unwrap();
        let mut stale_header = message("stale Gmail header", "must not publish");
        stale_header.account_id = account_id.to_string();
        stale_header.uid = 1;
        stale_header.mailbox = "Archive".into();
        assert!(store
            .commit_folder_header_batch_with_gmail_observations(
                &account_receipt,
                &stale_epoch,
                "Archive",
                &stale.generation,
                stale_revision,
                &[stale_header],
                &[GmailProviderObservation {
                    uid: 1,
                    gmail_message_id: "9002".into(),
                    labels: vec!["\\Inbox".into()],
                }],
                None,
                true,
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("Gmail Inbox membership changed"));
        assert!(store
            .message_by_locator(account_id, "Archive", 1)
            .await
            .unwrap()
            .is_none());
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
        self.begin_mailbox_snapshot_inner(account_id, mailbox, identity, None)
            .await
    }

    /// Captures the exact account configuration before a full mailbox
    /// snapshot starts. The resulting generation refuses to stage or publish
    /// after that account's provider endpoint or principal changes.
    pub async fn begin_mailbox_snapshot_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        identity: MailboxSnapshotIdentity<'_>,
    ) -> Result<String> {
        let receipt = self
            .capture_account_provider_receipt(account)
            .await?
            .ok_or_else(|| anyhow!("account provider configuration is no longer current"))?;
        self.begin_mailbox_snapshot_with_provider_receipt(&receipt, mailbox, identity)
            .await
    }

    /// Starts a snapshot with a receipt retained by the protocol before its
    /// first IMAP command.
    pub async fn begin_mailbox_snapshot_with_provider_receipt(
        &self,
        receipt: &AccountProviderReceipt,
        mailbox: &str,
        identity: MailboxSnapshotIdentity<'_>,
    ) -> Result<String> {
        self.begin_mailbox_snapshot_inner(receipt.account_id, mailbox, identity, Some(receipt))
            .await
    }

    async fn begin_mailbox_snapshot_inner(
        &self,
        account_id: AccountId,
        mailbox: &str,
        identity: MailboxSnapshotIdentity<'_>,
        receipt: Option<&AccountProviderReceipt>,
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
        let mut uid_next = uid_next.map(i64::from);
        let highest_modseq = highest_modseq.map(|value| value.to_string());
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(receipt) = receipt {
            ensure_account_provider_receipt_current_in_transaction(&mut tx, receipt).await?;
        }
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
        // Some servers omit UIDNEXT. The initial response still gives us a
        // fixed safe boundary for already committed UIDs: IMAP UIDs only grow,
        // so a complete inventory can reconcile every UID below the largest
        // locator known when this generation began. Never extend this bound
        // with rows that may arrive while the inventory is running.
        if uid_next.is_none() {
            uid_next = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT MAX(uid) + 1 FROM messages WHERE account_id = ? AND mailbox = ?",
            )
            .bind(&account_id)
            .bind(mailbox)
            .fetch_one(&mut *tx)
            .await?;
        }
        let existing: Option<(String, String, i64, i64, Option<i64>, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT generation, remote_name, uid_validity, initial_exists, uid_next, account_config_generation, account_config_fingerprint FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ?",
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
            existing_config_generation,
            existing_config_fingerprint,
        )) = existing
        {
            if existing_remote_name == remote_name
                && existing_uid_validity == i64::from(uid_validity)
                && existing_exists == i64::from(initial_exists)
                && existing_uid_next == uid_next
                && receipt.is_none_or(|receipt| {
                    existing_config_generation == Some(receipt.config_generation)
                        && existing_config_fingerprint.as_deref()
                            == Some(receipt.config_fingerprint.as_str())
                })
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
        sqlx::query("INSERT INTO mailbox_snapshot_generations(account_id, mailbox, generation, remote_name, uid_validity, initial_exists, uid_next, highest_modseq, account_config_generation, account_config_fingerprint, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
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
            .bind(receipt.map(|receipt| receipt.config_generation))
            .bind(receipt.map(|receipt| receipt.config_fingerprint.as_str()))
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
        let publication_timer = PublicationTransactionTimer::start();
        let active: Option<(i64, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT uid_validity, account_config_generation, account_config_fingerprint FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((snapshot_uid_validity, config_generation, config_fingerprint)) = active else {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
        let now = Utc::now();
        for (uid, is_read, is_flagged) in flags {
            let local_mutation_version: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM mailbox_mutation_versions WHERE account_id = ? AND mailbox = ? AND uid = ? AND uid_validity = ?")
                .bind(&account_id).bind(mailbox).bind(i64::from(*uid)).bind(snapshot_uid_validity).fetch_one(&mut *tx).await?;
            sqlx::query("INSERT INTO mailbox_snapshot_items(account_id, mailbox, generation, uid, is_read, is_flagged, local_mutation_version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, generation, uid) DO UPDATE SET is_read=excluded.is_read, is_flagged=excluded.is_flagged, local_mutation_version=excluded.local_mutation_version, updated_at=excluded.updated_at")
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .bind(i64::from(*uid))
                .bind(*is_read)
                .bind(*is_flagged)
                .bind(local_mutation_version)
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
        publication_timer.committed();
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
        let publication_timer = PublicationTransactionTimer::start();
        let active: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT account_config_generation, account_config_fingerprint FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((config_generation, config_fingerprint)) = active else {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
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
        publication_timer.committed();
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
        let publication_timer = PublicationTransactionTimer::start();
        let receipt: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT account_config_generation, account_config_fingerprint FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((config_generation, config_fingerprint)) = receipt else {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
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
        publication_timer.committed();
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
            None,
        )
        .await
    }

    /// Publishes a snapshot only when the account receipt captured before its
    /// IMAP work still matches the generation and the persisted account.
    pub async fn finalize_mailbox_snapshot_with_provider_receipt(
        &self,
        receipt: &AccountProviderReceipt,
        mailbox: &str,
        generation: &str,
    ) -> Result<u64> {
        self.finalize_mailbox_snapshot_inner(
            receipt.account_id,
            mailbox,
            generation,
            SnapshotReplacementPublication::None,
            SnapshotFinalizeWatermark::StagedMaximum,
            Some(receipt),
        )
        .await
    }

    /// Convenience form for code retaining the exact Account that issued the
    /// snapshot's provider requests.
    pub async fn finalize_mailbox_snapshot_for_account(
        &self,
        account: &Account,
        mailbox: &str,
        generation: &str,
    ) -> Result<u64> {
        let receipt = self
            .capture_account_provider_receipt(account)
            .await?
            .ok_or_else(|| anyhow!("account provider configuration is no longer current"))?;
        self.finalize_mailbox_snapshot_with_provider_receipt(&receipt, mailbox, generation)
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
        receipt: Option<&AccountProviderReceipt>,
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
        let publication_timer = PublicationTransactionTimer::start();
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
            "SELECT remote_name, uid_validity, initial_exists, uid_next, highest_modseq, account_config_generation, account_config_fingerprint FROM mailbox_snapshot_generations WHERE account_id = ? AND mailbox = ? AND generation = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((
            remote_name,
            uid_validity,
            initial_exists,
            uid_next,
            highest_modseq,
            config_generation,
            config_fingerprint,
        )) = generation_state
        else {
            tx.rollback().await?;
            return Err(anyhow!("mailbox snapshot generation is not active"));
        };
        ensure_stored_provider_receipt_current_in_transaction(
            &mut tx,
            &account_id,
            config_generation,
            config_fingerprint.as_deref(),
        )
        .await?;
        if let Some(receipt) = receipt {
            ensure_account_provider_receipt_current_in_transaction(&mut tx, receipt).await?;
            if receipt.account_id.to_string() != account_id
                || config_generation != Some(receipt.config_generation)
                || config_fingerprint.as_deref() != Some(receipt.config_fingerprint.as_str())
            {
                tx.rollback().await?;
                return Err(anyhow!(
                    "mailbox snapshot generation belongs to another provider configuration"
                ));
            }
        }
        let prior_uid_validities: Vec<i64> = sqlx::query_scalar(
            "SELECT uid_validity FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? AND historical_complete = 1 UNION SELECT uid_validity FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ? AND uid_validity IS NOT NULL",
        )
        .bind(&account_id)
        .bind(mailbox)
        .bind(&account_id)
        .bind(mailbox)
        .fetch_all(&mut *tx)
        .await?;
        let prior_catalog_uid_next: Option<i64> = sqlx::query_scalar(
            "SELECT uid_next FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(&account_id)
        .bind(mailbox)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
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
            // A pending operation belongs to the old UID namespace. It must
            // never fence a recycled UID after this atomic publication.
            sqlx::query("UPDATE operation_journal SET state = 'uncertain', outcome = COALESCE(outcome, 'uidvalidity_changed'), error = COALESCE(error, 'mailbox UIDVALIDITY changed before operation completed'), updated_at = ? WHERE account_id = ? AND mailbox = ? AND uid_validity IS NOT NULL AND uid_validity != ? AND state NOT IN ('completed', 'rejected', 'permanent_failed', 'uncertain')")
                .bind(Utc::now()).bind(&account_id).bind(mailbox).bind(uid_validity).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM mailbox_mutation_fences WHERE account_id = ? AND mailbox = ? AND (uid_validity IS NULL OR uid_validity != ?)")
                .bind(&account_id).bind(mailbox).bind(uid_validity).execute(&mut *tx).await?;
        }
        match replacement_publication {
            SnapshotReplacementPublication::None => {}
            SnapshotReplacementPublication::InMemory(replacements) => {
                for replacement in replacements {
                    let mut replacement = replacement.clone();
                    replacement.id = replacement_message_id(
                        &account_id,
                        mailbox,
                        replacement.uid,
                        uid_validity,
                    )?;
                    for attachment in &mut replacement.attachments {
                        attachment.attachment.message_id = replacement.id.clone();
                    }
                    persist_message(&mut tx, &replacement).await?;
                }
            }
            SnapshotReplacementPublication::Staged => {
                persist_staged_uidvalidity_replacement_messages_in_transaction(
                    &mut tx,
                    &account_id,
                    mailbox,
                    generation,
                    uid_validity,
                )
                .await?;
            }
        }
        sqlx::query("UPDATE messages SET is_read = (SELECT staged.is_read FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?), is_flagged = (SELECT staged.is_flagged FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?) WHERE account_id = ? AND mailbox = ? AND EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?) AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_fences AS fence WHERE fence.account_id = messages.account_id AND fence.mailbox = messages.mailbox AND fence.uid = messages.uid AND (fence.uid_validity IS NULL OR fence.uid_validity = ?)) AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_versions AS version WHERE version.account_id = messages.account_id AND version.mailbox = messages.mailbox AND version.uid = messages.uid AND version.uid_validity = ? AND version.version > (SELECT staged.local_mutation_version FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.uid = messages.uid AND staged.generation = ?))")
            .bind(generation)
            .bind(generation)
            .bind(&account_id)
            .bind(mailbox)
            .bind(generation)
            .bind(uid_validity)
            .bind(uid_validity)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        // Cache ownership follows the effective flags after the guarded
        // publication. A staged older flag must never evict cache content
        // preserved by a newer local mutation version.
        sqlx::query("DELETE FROM message_content_cache WHERE message_id IN (SELECT message.id FROM messages AS message JOIN mailbox_snapshot_items AS staged ON staged.account_id = message.account_id AND staged.mailbox = message.mailbox AND staged.uid = message.uid WHERE staged.account_id = ? AND staged.mailbox = ? AND staged.generation = ? AND message.is_flagged = 1)")
            .bind(&account_id).bind(mailbox).bind(generation).execute(&mut *tx).await?;
        for table in ["starred_message_bodies", "starred_attachment_metadata"] {
            let statement = format!("DELETE FROM {table} WHERE message_id IN (SELECT message.id FROM messages AS message JOIN mailbox_snapshot_items AS staged ON staged.account_id = message.account_id AND staged.mailbox = message.mailbox AND staged.uid = message.uid WHERE staged.account_id = ? AND staged.mailbox = ? AND staged.generation = ? AND message.is_flagged = 0)");
            sqlx::query(&statement)
                .bind(&account_id)
                .bind(mailbox)
                .bind(generation)
                .execute(&mut *tx)
                .await?;
        }
        // UIDNEXT captures the inclusive reconciliation boundary. A full
        // inventory without it can update observed rows but cannot prove an
        // absence safely, and a realtime row above the boundary is never
        // eligible for deletion. Pending optimistic mutations are similarly
        // fenced until their operation journal outcome is reconciled.
        // An initial empty mailbox reports UIDNEXT=1. Its complete inventory
        // is authoritative for inherited rows from before catalogue state was
        // introduced. Every other snapshot remains bounded by UIDNEXT so a
        // realtime row at or above the captured boundary survives.
        let reconciliation_uid_next =
            if prior_uid_validities.is_empty() && initial_exists == 0 && uid_next == Some(1) {
                Some(i64::MAX)
            } else {
                match (uid_next, prior_catalog_uid_next) {
                    (Some(current), Some(prior)) => Some(current.max(prior)),
                    (current, None) => current,
                    (None, prior) => prior,
                }
            };
        let deleted = sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ? AND ? IS NOT NULL AND uid < ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = messages.account_id AND staged.mailbox = messages.mailbox AND staged.generation = ? AND staged.uid = messages.uid) AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_fences AS fence WHERE fence.account_id = messages.account_id AND fence.mailbox = messages.mailbox AND fence.uid = messages.uid AND (fence.uid_validity IS NULL OR fence.uid_validity = ?))")
            .bind(&account_id)
            .bind(mailbox)
            .bind(reconciliation_uid_next)
            .bind(reconciliation_uid_next)
            .bind(generation)
            .bind(uid_validity)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // A complete authoritative snapshot also proves that failures for
        // UIDs no longer present remotely are obsolete. Failures for staged
        // UIDs remain until their metadata fetch succeeds explicitly.
        sqlx::query("DELETE FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND ? IS NOT NULL AND uid < ? AND NOT EXISTS (SELECT 1 FROM mailbox_snapshot_items AS staged WHERE staged.account_id = mailbox_sync_failures.account_id AND staged.mailbox = mailbox_sync_failures.mailbox AND staged.generation = ? AND staged.uid = mailbox_sync_failures.uid)")
            .bind(&account_id)
            .bind(mailbox)
            .bind(uid_next)
            .bind(uid_next)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, uid_next, highest_modseq, provider_config_generation, provider_config_fingerprint, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, remote_total=excluded.remote_total, historical_complete=excluded.historical_complete, uid_next=excluded.uid_next, highest_modseq=excluded.highest_modseq, provider_config_generation=excluded.provider_config_generation, provider_config_fingerprint=excluded.provider_config_fingerprint, updated_at=excluded.updated_at")
            .bind(&account_id)
            .bind(mailbox)
            .bind(remote_name)
            .bind(uid_validity)
            .bind(initial_exists)
            .bind(uid_next)
            .bind(highest_modseq)
            .bind(config_generation)
            .bind(&config_fingerprint)
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
        publication_timer.committed();
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

    /// Records a sparse retry item without making a failed UID look absent.
    /// The scheduler can pause authentication failures while continuing other
    /// folders and can isolate malformed messages by their durable class.
    pub async fn record_mailbox_sync_failure_with_retry(
        &self,
        account_id: AccountId,
        mailbox: &str,
        uid: u32,
        stage: &str,
        error_class: &str,
        error: &str,
        next_retry_at: Option<DateTime<Utc>>,
        user_action_required: bool,
    ) -> Result<()> {
        if uid == 0 || error_class.trim().is_empty() {
            return Err(anyhow!(
                "mailbox retry record has invalid UID or error class"
            ));
        }
        sqlx::query("INSERT INTO mailbox_sync_failures(account_id, mailbox, uid, stage, error, error_class, attempt_count, next_retry_at, user_action_required, updated_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET stage=excluded.stage, error=excluded.error, error_class=excluded.error_class, attempt_count=mailbox_sync_failures.attempt_count+1, next_retry_at=excluded.next_retry_at, user_action_required=excluded.user_action_required, updated_at=excluded.updated_at")
            .bind(account_id.to_string()).bind(mailbox).bind(i64::from(uid)).bind(stage).bind(error).bind(error_class).bind(next_retry_at).bind(user_action_required).bind(Utc::now())
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Header coverage cannot be complete while these failures remain. A
    /// folder with no due work can yield to other stages, then wake at the
    /// earliest retry. Content/preview failures do not block header coverage.
    pub async fn mailbox_header_failure_summary(
        &self,
        account_id: AccountId,
        mailbox: &str,
    ) -> Result<MailboxFailureSummary> {
        let (outstanding, retryable, user_action_required, next_retry_at): (i64, i64, i64, Option<DateTime<Utc>>) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(CASE WHEN user_action_required = 0 THEN 1 ELSE 0 END), 0), COALESCE(SUM(CASE WHEN user_action_required = 1 THEN 1 ELSE 0 END), 0), MIN(CASE WHEN user_action_required = 0 THEN COALESCE(next_retry_at, ?) ELSE NULL END) FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND stage IN ('headers', 'metadata')")
            .bind(Utc::now()).bind(account_id.to_string()).bind(mailbox).fetch_one(&self.pool).await?;
        Ok(MailboxFailureSummary {
            outstanding: outstanding.try_into()?,
            retryable: retryable.try_into()?,
            user_action_required: user_action_required.try_into()?,
            next_retry_at,
        })
    }

    pub async fn due_mailbox_sync_failures(
        &self,
        account_id: AccountId,
        mailbox: &str,
        limit: usize,
    ) -> Result<Vec<MailboxSyncFailure>> {
        Ok(sqlx::query_as("SELECT account_id, mailbox, uid, stage, error, error_class, attempt_count, next_retry_at, user_action_required, updated_at FROM mailbox_sync_failures WHERE account_id = ? AND mailbox = ? AND user_action_required = 0 AND (next_retry_at IS NULL OR next_retry_at <= ?) ORDER BY next_retry_at, updated_at, uid LIMIT ?")
            .bind(account_id.to_string()).bind(mailbox).bind(Utc::now()).bind(i64::try_from(limit.clamp(1, 500))?)
            .fetch_all(&self.pool).await?)
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
        self.apply_complete_mailbox_changed_since_flags_inner(
            account_id,
            mailbox,
            changed_since,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Publishes a CONDSTORE delta using the receipt captured before the
    /// network command. `false` means the selected namespace changed.
    pub async fn apply_complete_mailbox_changed_since_flags_with_receipt(
        &self,
        receipt: &ChangedSinceWriteReceipt,
        changed_since: MailboxChangedSinceFlags<'_>,
    ) -> Result<bool> {
        if changed_since.identity.remote_name != receipt.remote_name
            || changed_since.identity.uid_validity != receipt.uid_validity
        {
            return Ok(false);
        }
        self.apply_complete_mailbox_changed_since_flags_inner(
            receipt.account_id,
            &receipt.mailbox,
            changed_since,
            Some(receipt),
        )
        .await
    }

    async fn apply_complete_mailbox_changed_since_flags_inner(
        &self,
        account_id: AccountId,
        mailbox: &str,
        changed_since: MailboxChangedSinceFlags<'_>,
        receipt: Option<&ChangedSinceWriteReceipt>,
    ) -> Result<bool> {
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
        if let Some(receipt) = receipt {
            let catalogue_stamp_current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ? AND remote_name = ? AND uid_validity = ? AND provider_config_generation = ? AND provider_config_fingerprint = ?)")
                .bind(&account_id)
                .bind(mailbox)
                .bind(&receipt.remote_name)
                .bind(i64::from(receipt.uid_validity))
                .bind(receipt.account_config_generation)
                .bind(&receipt.account_config_fingerprint)
                .fetch_one(&mut *tx)
                .await?;
            if !catalogue_stamp_current {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        if current.as_ref() != Some(&(remote_name.to_owned(), i64::from(uid_validity), true)) {
            tx.rollback().await?;
            if receipt.is_some() {
                return Ok(false);
            }
            return Err(anyhow!(
                "cannot apply CHANGEDSINCE delta without a stable mailbox catalogue"
            ));
        }
        if let Some(receipt) = receipt {
            let account_current: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ? AND config_generation = ? AND config_fingerprint = ? AND NOT EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?))",
            )
            .bind(&account_id)
            .bind(receipt.account_config_generation)
            .bind(&receipt.account_config_fingerprint)
            .bind(&account_id)
            .fetch_one(&mut *tx)
            .await?;
            if !account_current {
                tx.rollback().await?;
                return Ok(false);
            }
        }
        let expected_versions: HashMap<i64, i64> = receipt
            .map(|receipt| {
                receipt
                    .local_mutation_versions
                    .iter()
                    .map(|version| (version.uid, version.version))
                    .collect()
            })
            .unwrap_or_default();
        for (uid, is_read, is_flagged) in flags {
            let expected_version = expected_versions.get(&i64::from(*uid)).copied();
            let applied = sqlx::query("UPDATE messages SET is_read = ?, is_flagged = ? WHERE account_id = ? AND mailbox = ? AND uid = ? AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_fences AS fence WHERE fence.account_id = messages.account_id AND fence.mailbox = messages.mailbox AND fence.uid = messages.uid AND (fence.uid_validity IS NULL OR fence.uid_validity = ?)) AND NOT EXISTS (SELECT 1 FROM mailbox_mutation_versions AS version WHERE version.account_id = messages.account_id AND version.mailbox = messages.mailbox AND version.uid = messages.uid AND version.uid_validity = ? AND version.version != ?)")
                .bind(is_read)
                .bind(is_flagged)
                .bind(&account_id)
                .bind(mailbox)
                .bind(i64::from(*uid))
                .bind(i64::from(uid_validity))
                .bind(i64::from(uid_validity))
                .bind(expected_version.unwrap_or(0))
                .execute(&mut *tx).await?.rows_affected();
            if applied != 1 {
                continue;
            }
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
        Ok(true)
    }
}
