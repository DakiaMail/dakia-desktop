use crate::{account::Account, provider, AccountAuth, AccountId};
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
    fs::OpenOptions,
    io::{ErrorKind, Write},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const VAULT_KEY_FILE: &str = "vault.key";
const VAULT_KEY_LEN: usize = 32;
const VAULT_NONCE_LEN: usize = 12;
/// Foreground-opened, non-starred mail is useful offline, but it must never
/// grow into a second unbounded mail store. Starred mail has its own durable,
/// authoritative cache and is intentionally not counted here.
const MESSAGE_CONTENT_CACHE_MAX_ENTRIES: i64 = 64;
const MESSAGE_CONTENT_CACHE_MAX_BYTES: i64 = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    vault_key: Arc<[u8; VAULT_KEY_LEN]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRebuildJob {
    pub account_id: AccountId,
    pub phase: String,
    pub completed: usize,
    pub total: Option<usize>,
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

#[derive(Debug, Clone)]
pub struct MailboxSyncState {
    pub initialized: bool,
    pub highest_uid: Option<u32>,
    pub uid_validity: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailConversation {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub messages: Vec<MailSummary>,
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
    pub is_potentially_unsafe: bool,
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
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self { pool, vault_key };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        let store = Self {
            pool,
            vault_key: Arc::new(random_bytes()?),
        };
        store.migrate().await?;
        Ok(store)
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
            "CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, message_id TEXT, in_reply_to TEXT, reference_ids TEXT, thread_id TEXT NOT NULL, threading_scanned INTEGER NOT NULL DEFAULT 1, recipient_headers_scanned INTEGER NOT NULL DEFAULT 1, subject TEXT NOT NULL, from_name TEXT, from_address TEXT NOT NULL, to_addresses TEXT NOT NULL, cc_addresses TEXT NOT NULL DEFAULT '', bcc_addresses TEXT NOT NULL DEFAULT '', reply_to_addresses TEXT NOT NULL DEFAULT '', received_at TEXT NOT NULL, snippet TEXT NOT NULL, body_text TEXT NOT NULL, unsubscribe_kind TEXT, unsubscribe_url TEXT, unsubscribe_scanned INTEGER NOT NULL DEFAULT 0, is_read INTEGER NOT NULL DEFAULT 0, is_flagged INTEGER NOT NULL DEFAULT 0, has_attachments INTEGER NOT NULL DEFAULT 0, category TEXT, classification_confidence REAL, classification_source TEXT, classification_signals TEXT NOT NULL DEFAULT '', UNIQUE(account_id, mailbox, uid))",
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, from_name, from_address, to_addresses, body_text, content='messages', content_rowid='rowid')",
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); END",
            "CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
            "CREATE INDEX IF NOT EXISTS messages_account_mailbox_date ON messages(account_id, mailbox, received_at DESC)",
            "CREATE TABLE IF NOT EXISTS mailbox_sync_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, initialized_at TEXT NOT NULL, highest_uid INTEGER, uid_validity INTEGER, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS mailbox_action_tombstones (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
            "CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, is_potentially_unsafe INTEGER NOT NULL DEFAULT 0, data BLOB NOT NULL)",
            "CREATE INDEX IF NOT EXISTS attachments_message_id ON attachments(message_id)",
            "CREATE TABLE IF NOT EXISTS starred_message_bodies (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, body_text TEXT NOT NULL, body_html TEXT, cached_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS starred_attachment_metadata (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, is_potentially_unsafe INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS message_content_cache (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, content_state TEXT NOT NULL CHECK(content_state = 'complete'), body_text TEXT NOT NULL, body_html TEXT, unsubscribe_kind TEXT, attachments_json TEXT NOT NULL, byte_size INTEGER NOT NULL CHECK(byte_size >= 0), last_accessed INTEGER NOT NULL)",
            "CREATE INDEX IF NOT EXISTS message_content_cache_lru ON message_content_cache(last_accessed, message_id)",
            "CREATE TABLE IF NOT EXISTS app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS mailbox_catalog_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, remote_total INTEGER NOT NULL DEFAULT 0, historical_complete INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
            "CREATE TABLE IF NOT EXISTS mail_rebuild_jobs (account_id TEXT PRIMARY KEY, phase TEXT NOT NULL, completed INTEGER NOT NULL DEFAULT 0, total INTEGER, updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS deleted_account_tombstones (account_id TEXT PRIMARY KEY, deleted_at TEXT NOT NULL)",
        ] {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("storage migration statement failed: {statement}"))?;
        }
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
            "CREATE TRIGGER IF NOT EXISTS mailbox_action_tombstones_require_account BEFORE INSERT ON mailbox_action_tombstones WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mailbox_catalog_state_require_account BEFORE INSERT ON mailbox_catalog_state WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
            "CREATE TRIGGER IF NOT EXISTS mail_rebuild_jobs_require_account BEFORE INSERT ON mail_rebuild_jobs WHEN EXISTS (SELECT 1 FROM deleted_account_tombstones WHERE account_id = NEW.account_id) BEGIN SELECT RAISE(ABORT, 'account was removed'); END",
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
            sqlx::query("UPDATE messages SET thread_id = id WHERE thread_id IS NULL")
                .execute(&self.pool)
                .await?;
        }
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
        if !sync_state_exists {
            sqlx::query("INSERT OR IGNORE INTO mailbox_sync_state(account_id, mailbox, initialized_at) SELECT id, 'INBOX', ? FROM accounts")
                .bind(Utc::now())
                .execute(&self.pool)
                .await?;
        }
        if migrated_legacy_profile {
            self.restore_legacy_desktop_profile().await?;
        }
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
                 UNION SELECT account_id FROM mailbox_action_tombstones \
                 UNION SELECT account_id FROM mail_rebuild_jobs \
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
            "DELETE FROM mailbox_action_tombstones WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mailbox_action_tombstones.account_id)",
            "DELETE FROM mail_rebuild_jobs WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.id = mail_rebuild_jobs.account_id)",
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

    pub async fn delete_secret(&self, name: &str) -> Result<()> {
        sqlx::query("DELETE FROM credentials WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_account(&self, account: &Account) -> Result<()> {
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
        sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET email=excluded.email, data=excluded.data")
            .bind(account.id.to_string())
            .bind(&account.email)
            .bind(serde_json::to_string(account)?)
            .bind(account.created_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
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
        let rows: Vec<(String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT account_id, phase, completed, total FROM mail_rebuild_jobs ORDER BY updated_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(account_id, phase, completed, total)| {
                Ok(MailRebuildJob {
                    account_id: AccountId::parse_str(&account_id)?,
                    phase,
                    completed: usize::try_from(completed)?,
                    total: total.map(usize::try_from).transpose()?,
                })
            })
            .collect()
    }

    pub async fn save_mail_rebuild_job(&self, job: &MailRebuildJob) -> Result<()> {
        sqlx::query(
            "INSERT INTO mail_rebuild_jobs(account_id, phase, completed, total, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(account_id) DO UPDATE SET phase=excluded.phase, completed=excluded.completed, total=excluded.total, updated_at=excluded.updated_at",
        )
        .bind(job.account_id.to_string())
        .bind(&job.phase)
        .bind(i64::try_from(job.completed)?)
        .bind(job.total.map(i64::try_from).transpose()?)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
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
        sqlx::query("INSERT INTO deleted_account_tombstones(account_id, deleted_at) VALUES (?, ?) ON CONFLICT(account_id) DO UPDATE SET deleted_at=excluded.deleted_at")
            .bind(id.to_string())
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
        sqlx::query("DELETE FROM mailbox_sync_state WHERE account_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mailbox_action_tombstones WHERE account_id = ?")
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
        Ok(())
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
            "DELETE FROM mailbox_sync_state WHERE account_id = ?",
            "DELETE FROM mailbox_action_tombstones WHERE account_id = ?",
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

    /// Stores one incremental mailbox batch and returns messages eligible for
    /// new-mail notification. The first successful batch establishes a silent
    /// baseline so connecting an account never alerts for historical mail.
    pub async fn save_synced_messages(
        &self,
        account_id: AccountId,
        mailbox: &str,
        messages: &[MailSummary],
    ) -> Result<Vec<MailSummary>> {
        let account_id = account_id.to_string();
        // Keep the existence check and all provider-derived writes in one
        // transaction.  This prevents a late realtime cycle from reporting
        // arrivals for an account removed between its IMAP fetch and local
        // publication.
        let mut tx = self.pool.begin().await?;
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
        let highest_uid = messages.iter().map(|message| message.uid).max();
        sqlx::query("INSERT INTO mailbox_sync_state(account_id, mailbox, initialized_at, highest_uid) VALUES (?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET initialized_at=excluded.initialized_at, highest_uid=MAX(COALESCE(mailbox_sync_state.highest_uid, 0), COALESCE(excluded.highest_uid, 0))")
            .bind(&account_id)
            .bind(mailbox)
            .bind(Utc::now())
            .bind(highest_uid)
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
            "SELECT account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete FROM mailbox_catalog_state WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_optional(&self.pool)
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
        sqlx::query("INSERT INTO mailbox_catalog_state(account_id, mailbox, remote_name, uid_validity, remote_total, historical_complete, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET remote_name=excluded.remote_name, uid_validity=excluded.uid_validity, remote_total=excluded.remote_total, historical_complete=excluded.historical_complete, updated_at=excluded.updated_at")
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(remote_name)
            .bind(uid_validity)
            .bind(remote_total as i64)
            .bind(historical_complete)
            .bind(Utc::now())
            .execute(&self.pool)
            .await?;
        Ok(())
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

    /// Reconciles the durable UIDVALIDITY/watermark before incremental sync.
    /// A changed UIDVALIDITY invalidates only this local mailbox and resets its
    /// notification baseline so historical mail is never reported as new.
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
        let changed = row
            .as_ref()
            .and_then(|(_, _, stored)| *stored)
            .zip(uid_validity.and_then(|value| i64::try_from(value).ok()))
            .is_some_and(|(stored, current)| stored != current);
        if changed {
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM messages WHERE account_id = ? AND mailbox = ?")
                .bind(&account_id)
                .bind(mailbox)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM mailbox_sync_state WHERE account_id = ? AND mailbox = ?")
                .bind(&account_id)
                .bind(mailbox)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = ?",
            )
            .bind(&account_id)
            .bind(mailbox)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(MailboxSyncState {
                initialized: false,
                highest_uid: None,
                uid_validity,
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
        const SQL: &str = "SELECT id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals FROM messages WHERE id = ?";
        let mut message = sqlx::query_as::<_, MailSummary>(SQL)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(message) = message.as_mut() {
            if let Some((body_text, body_html)) = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT body_text, body_html FROM starred_message_bodies WHERE message_id = ?",
            )
            .bind(id)
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
        Ok(sqlx::query_as::<_, Attachment>("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe FROM starred_attachment_metadata WHERE message_id = ? ORDER BY filename, id")
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn starred_body(&self, message_id: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(sqlx::query_as(
            "SELECT body_text, body_html FROM starred_message_bodies WHERE message_id = ?",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?)
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
        let attachments = serde_json::from_str(&attachments_json)
            .context("cached attachment metadata is invalid")?;
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
        let mut attachments = content.attachments;
        // The cache key is the current provider locator. Never retain parsed
        // metadata that refers to a different local message id.
        for attachment in &mut attachments {
            attachment.message_id = message_id.to_owned();
        }
        let attachments_json = serde_json::to_string(&attachments)?;
        let byte_size = cache_entry_byte_size(
            &content.body_text,
            content.body_html.as_deref(),
            content.unsubscribe_kind.as_deref(),
            &attachments_json,
        )?;
        if byte_size > MESSAGE_CONTENT_CACHE_MAX_BYTES {
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

        let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache")
            .fetch_one(&mut *tx)
            .await?;
        if entries > MESSAGE_CONTENT_CACHE_MAX_ENTRIES {
            sqlx::query(
                "DELETE FROM message_content_cache WHERE message_id IN (SELECT message_id FROM (SELECT message_id FROM message_content_cache ORDER BY last_accessed, message_id LIMIT ?))",
            )
            .bind(entries - MESSAGE_CONTENT_CACHE_MAX_ENTRIES)
            .execute(&mut *tx)
            .await?;
        }
        loop {
            let used_bytes: i64 =
                sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM message_content_cache")
                    .fetch_one(&mut *tx)
                    .await?;
            if used_bytes <= MESSAGE_CONTENT_CACHE_MAX_BYTES {
                break;
            }
            let removed = sqlx::query(
                "DELETE FROM message_content_cache WHERE message_id = (SELECT message_id FROM message_content_cache ORDER BY last_accessed, message_id LIMIT 1)",
            )
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
        const SQL: &str = "SELECT m.id, m.account_id, m.mailbox, m.uid, m.message_id, m.in_reply_to, m.reference_ids, m.thread_id, m.subject, m.from_name, m.from_address, m.to_addresses, m.cc_addresses, m.bcc_addresses, m.reply_to_addresses, m.received_at, m.snippet, m.body_text, m.body_html, m.content_state, m.unsubscribe_kind, m.unsubscribe_url, m.is_read, m.is_flagged, m.has_attachments, m.category, m.classification_confidence, m.classification_source, m.classification_signals FROM messages m LEFT JOIN starred_message_bodies b ON b.message_id = m.id WHERE m.account_id = ? AND m.is_flagged = 1 AND b.message_id IS NULL ORDER BY m.received_at DESC LIMIT ?";
        Ok(sqlx::query_as::<_, MailSummary>(SQL)
            .bind(account_id.to_string())
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
            .filter_map(|((account_id, thread_id), messages)| {
                let messages = deduplicate_message_copies(messages, query.mailbox.as_deref());
                let latest = messages.last()?.clone();
                let mut participants = messages
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
                    unread: messages.iter().any(|message| !message.is_read),
                    has_attachments: messages.iter().any(|message| message.has_attachments),
                    participants,
                    latest,
                    messages,
                })
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
            sqlx::query("UPDATE messages SET classification_signals = ? WHERE id = ?")
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
        classifications: &[(String, String, f64)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (id, category, confidence) in classifications {
            sqlx::query("UPDATE messages SET category = ?, classification_confidence = ?, classification_source = 'model' WHERE id = ? AND (classification_source IS NULL OR classification_source = 'model')")
                .bind(category)
                .bind(confidence)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>> {
        sqlx::query_as::<_, Attachment>("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe FROM attachments WHERE message_id = ? ORDER BY filename COLLATE NOCASE, id")
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
        let row: (String, String, String, String, i64, bool, bool, Vec<u8>) = sqlx::query_as("SELECT id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe, data FROM attachments WHERE message_id = ? AND id = ?")
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
                is_potentially_unsafe: row.6,
            },
            bytes: row.7,
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
        let rows = sqlx::query_as::<_, ThreadRow>("SELECT id, message_id, in_reply_to, reference_ids, subject, from_address, to_addresses, received_at FROM messages WHERE account_id = ? ORDER BY received_at, id")
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
        let mut tx = self.pool.begin().await?;
        for (index, row) in rows.iter().enumerate() {
            let thread_id = &roots[&groups.find(index)];
            sqlx::query("UPDATE messages SET thread_id = ? WHERE id = ?")
                .bind(thread_id)
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
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
    message_id: Option<String>,
    in_reply_to: Option<String>,
    reference_ids: Option<String>,
    subject: String,
    from_address: String,
    to_addresses: String,
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
    mailparse::msgidparse(value)
        .map(|ids| ids.iter().map(|id| id.to_ascii_lowercase()).collect())
        .unwrap_or_default()
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

async fn persist_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    message: &MailSummary,
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
    sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account_id, mailbox, uid) DO UPDATE SET message_id=excluded.message_id, in_reply_to=excluded.in_reply_to, reference_ids=excluded.reference_ids, threading_scanned=1, recipient_headers_scanned=1, subject=excluded.subject, from_name=excluded.from_name, from_address=excluded.from_address, to_addresses=excluded.to_addresses, cc_addresses=excluded.cc_addresses, bcc_addresses=excluded.bcc_addresses, reply_to_addresses=excluded.reply_to_addresses, received_at=excluded.received_at, snippet=CASE WHEN excluded.content_state = 'complete' THEN excluded.snippet ELSE messages.snippet END, body_text=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_text ELSE messages.body_text END, body_html=CASE WHEN excluded.content_state = 'complete' THEN excluded.body_html ELSE messages.body_html END, content_state=CASE WHEN messages.content_state = 'complete' THEN messages.content_state ELSE excluded.content_state END, unsubscribe_kind=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_kind ELSE messages.unsubscribe_kind END, unsubscribe_url=CASE WHEN excluded.content_state = 'complete' THEN excluded.unsubscribe_url ELSE messages.unsubscribe_url END, unsubscribe_scanned=CASE WHEN excluded.content_state = 'complete' THEN 1 ELSE messages.unsubscribe_scanned END, is_read=excluded.is_read, is_flagged=excluded.is_flagged, has_attachments=CASE WHEN excluded.content_state = 'complete' THEN excluded.has_attachments ELSE messages.has_attachments END, classification_signals=excluded.classification_signals")
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
        .execute(&mut **tx).await?;
    if message.is_flagged && message.content_state == "complete" {
        sqlx::query("INSERT INTO starred_message_bodies(message_id, body_text, body_html, cached_at) VALUES (?, ?, ?, ?) ON CONFLICT(message_id) DO UPDATE SET body_text=excluded.body_text, body_html=excluded.body_html, cached_at=excluded.cached_at")
            .bind(&message.id)
            .bind(&message.body_text)
            .bind(&message.body_html)
            .bind(Utc::now())
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM starred_attachment_metadata WHERE message_id = ?")
            .bind(&message.id)
            .execute(&mut **tx)
            .await?;
        for attachment in &message.attachments {
            sqlx::query("INSERT INTO starred_attachment_metadata(id, message_id, filename, mime_type, size_bytes, is_inline, is_potentially_unsafe) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&attachment.attachment.id)
                .bind(&message.id)
                .bind(&attachment.attachment.filename)
                .bind(&attachment.attachment.mime_type)
                .bind(attachment.attachment.size_bytes)
                .bind(attachment.attachment.is_inline)
                .bind(attachment.attachment.is_potentially_unsafe)
                .execute(&mut **tx)
                .await?;
        }
    } else if !message.is_flagged {
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
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(path) {
                Ok(mut file) => {
                    file.write_all(&key)?;
                    file.sync_all()?;
                    Ok(key)
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

pub fn stable_message_id(account_id: AccountId, mailbox: &str, uid: u32) -> String {
    // UUID v4 is used for accounts; deriving a stable ID avoids duplicates during resync.
    format!("{}:{}:{}", account_id, mailbox.replace(':', "_"), uid)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        store.save_mail_rebuild_job(&job).await.unwrap();
        let restored = store.mail_rebuild_jobs().await.unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].account_id, account_id);
        assert_eq!(restored[0].phase, "downloading");
        assert_eq!(restored[0].completed, 150);
        assert_eq!(restored[0].total, Some(1_200));

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

    fn cached_content(body_text: &str) -> CachedMessageContent {
        CachedMessageContent {
            body_text: body_text.into(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: Vec::new(),
        }
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
    async fn body_cache_replaces_accounting_and_evicts_entry_and_byte_lru() {
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

        for index in 0..=MESSAGE_CONTENT_CACHE_MAX_ENTRIES {
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
            .is_none());
        assert!(store
            .cached_message_content(&second_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content(&format!("entry-{MESSAGE_CONTENT_CACHE_MAX_ENTRIES}"))
            .await
            .unwrap()
            .is_some());
        let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_content_cache")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(entries, MESSAGE_CONTENT_CACHE_MAX_ENTRIES);

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
        let half = "x".repeat((MESSAGE_CONTENT_CACHE_MAX_BYTES / 2) as usize);
        byte_store
            .cache_message_content(&byte_first_id, false, cached_content(&half))
            .await
            .unwrap();
        byte_store
            .cache_message_content(&byte_second_id, false, cached_content(&half))
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
        let exact = "x".repeat((MESSAGE_CONTENT_CACHE_MAX_BYTES - 2) as usize);
        store
            .cache_message_content(&id, false, cached_content(&exact))
            .await
            .unwrap();
        assert!(store.cached_message_content(&id).await.unwrap().is_some());
        let oversized = "x".repeat((MESSAGE_CONTENT_CACHE_MAX_BYTES + 1) as usize);
        store
            .cache_message_content(&id, false, cached_content(&oversized))
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
        assert!(store.cached_message_content(&id).await.is_err());
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
    async fn completed_mailbox_action_rejects_a_late_realtime_write() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
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
        sqlx::query("INSERT INTO messages(id, accountId, mailboxId, threadId, uid, messageId, inReplyTo, referencesJson, fromAddress, toAddresses, subject, date, flags, snippet) VALUES ('legacy-message', ?, 'inbox', NULL, 5, '<legacy@example.test>', NULL, '[]', 'sender@example.test', 'person@example.test', 'Legacy subject', 'Mon, 01 Jan 2024 12:00:00 +0000', '[\"\\\\Seen\"]', 'Legacy preview')")
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
    async fn notification_baseline_never_reports_non_inbox_mail() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
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
    async fn uidvalidity_change_resets_mailbox_and_notification_baseline() {
        let store = Store::in_memory().await.unwrap();
        let account_id = uuid::Uuid::new_v4();
        let mut old = message("Old UID namespace", "old");
        old.account_id = account_id.to_string();
        store
            .save_synced_messages(account_id, "INBOX", &[old])
            .await
            .unwrap();
        store
            .set_mailbox_uid_validity(account_id, "INBOX", Some(10))
            .await
            .unwrap();

        let reset = store
            .prepare_mailbox_sync(account_id, "INBOX", Some(11))
            .await
            .unwrap();
        assert!(!reset.initialized);
        assert_eq!(reset.highest_uid, None);

        let mut replacement = message("New UID namespace", "new");
        replacement.account_id = account_id.to_string();
        assert!(store
            .save_synced_messages(account_id, "INBOX", &[replacement])
            .await
            .unwrap()
            .is_empty());
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
        initial
            .save_synced_messages(orphan_id, "INBOX", &[orphan.clone()])
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
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'INBOX', 41, ?)")
            .bind(orphan_id.to_string())
            .bind(Utc::now())
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
}
