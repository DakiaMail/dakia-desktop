use super::{
    encrypt_secret, profile_migration_lock_path, random_bytes, VAULT_KEY_FILE, VAULT_KEY_LEN,
    VAULT_NONCE_LEN,
};
use crate::{
    account::AccountDraft,
    mail::ComposeAttachment,
    provider,
    storage::{
        MailRebuildJob, MailSummary, SendAttemptClaim, SendAttemptStatus, SendRecoveryClaim,
        SendRecoveryStatus,
    },
    Account, ComposeMessage, Store,
};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs, path::Path, str::FromStr, time::Duration};
use tempfile::tempdir;
use uuid::Uuid;

fn account(id: Uuid) -> Account {
    let mut account = AccountDraft {
        email: format!("fault-{id}@example.test"),
        display_name: "Fault fixture".into(),
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
    .into_account(provider::by_id("fastmail").expect("fixture provider"));
    account.id = id;
    account
}

fn message(account_id: Uuid, uid: i64, subject: &str, body: &str) -> MailSummary {
    MailSummary {
        id: format!("{account_id}:INBOX:{uid}"),
        account_id: account_id.to_string(),
        mailbox: "INBOX".into(),
        uid,
        mailbox_uid_validity: Some(7),
        message_id: Some(format!("<{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("thread-{uid}"),
        subject: subject.into(),
        from_name: Some("Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "recipient@example.test".into(),
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

fn recovery_message(account_id: Uuid, body: String) -> ComposeMessage {
    ComposeMessage {
        account_id,
        to: vec!["recovery-recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "SQLite recovery fault fixture".into(),
        body_text: body,
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![ComposeAttachment {
            filename: "recovery.txt".into(),
            mime_type: "text/plain".into(),
            content_base64: "ZmljdGlvbmFsIHJlY292ZXJ5".into(),
        }],
    }
}

fn profile_files(path: &Path) -> Vec<(OsString, Vec<u8>)> {
    let mut files = fs::read_dir(path)
        .expect("list profile directory")
        .map(|entry| {
            let entry = entry.expect("read profile entry");
            let name = entry.file_name();
            let contents = fs::read(entry.path()).expect("read profile file");
            (name, contents)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    sidecar.into()
}

async fn create_original_desktop_profile(path: &std::path::Path, account_id: &str, port: i64) {
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse legacy profile path")
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("create legacy profile");
    for statement in [
        "CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, displayName TEXT, host TEXT NOT NULL, port INTEGER NOT NULL, tls INTEGER NOT NULL, username TEXT NOT NULL, providerCapabilities TEXT NOT NULL, createdAt TEXT NOT NULL)",
        "CREATE TABLE mailboxes (id TEXT PRIMARY KEY, accountId TEXT NOT NULL, path TEXT NOT NULL, uidValidity INTEGER)",
        "CREATE TABLE messages (id TEXT PRIMARY KEY, accountId TEXT NOT NULL, mailboxId TEXT NOT NULL, threadId TEXT, uid INTEGER NOT NULL, messageId TEXT, inReplyTo TEXT, referencesJson TEXT NOT NULL, fromAddress TEXT NOT NULL, toAddresses TEXT NOT NULL, subject TEXT, date TEXT, flags TEXT NOT NULL, snippet TEXT)",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("legacy schema statement failed: {statement}: {error}"));
    }
    sqlx::query("INSERT INTO accounts(id, email, displayName, host, port, tls, username, providerCapabilities, createdAt) VALUES (?, 'migration@example.test', 'Migration fixture', 'imap.migadu.com', ?, 1, 'migration@example.test', '{}', '2026-01-01 00:00:00')")
        .bind(account_id)
        .bind(port)
        .execute(&pool)
        .await
        .expect("seed legacy account");
    sqlx::query("INSERT INTO mailboxes(id, accountId, path, uidValidity) VALUES ('legacy-inbox', ?, 'INBOX', 51)")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("seed legacy mailbox");
    sqlx::query("INSERT INTO messages(id, accountId, mailboxId, threadId, uid, messageId, inReplyTo, referencesJson, fromAddress, toAddresses, subject, date, flags, snippet) VALUES ('legacy-message', ?, 'legacy-inbox', NULL, 7, '<legacy@example.test>', NULL, '[]', 'sender@example.test', 'migration@example.test', 'Recoverable legacy message', 'Mon, 01 Jan 2024 12:00:00 +0000', '[]', 'Preserve this message')")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("seed legacy message");
    pool.close().await;
}

// Exact schema declarations from the published v0.4.4 storage migration
// (`v0.4.4:crates/dakia-core/src/storage.rs`, blob
// 21f697d21a10e71b015f1e896d9b8de74e2e560f). Keep this fixture tied to
// release provenance rather than reconstructing an imagined old profile.
const V0_4_4_SCHEMA: &[&str] = &[
    "CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, created_at TEXT NOT NULL)",
    "CREATE TABLE credentials (name TEXT PRIMARY KEY, nonce BLOB NOT NULL, ciphertext BLOB NOT NULL, updated_at TEXT NOT NULL)",
    "CREATE TABLE messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, message_id TEXT, in_reply_to TEXT, reference_ids TEXT, thread_id TEXT NOT NULL, threading_scanned INTEGER NOT NULL DEFAULT 1, recipient_headers_scanned INTEGER NOT NULL DEFAULT 1, subject TEXT NOT NULL, from_name TEXT, from_address TEXT NOT NULL, to_addresses TEXT NOT NULL, cc_addresses TEXT NOT NULL DEFAULT '', bcc_addresses TEXT NOT NULL DEFAULT '', reply_to_addresses TEXT NOT NULL DEFAULT '', received_at TEXT NOT NULL, snippet TEXT NOT NULL, body_text TEXT NOT NULL, unsubscribe_kind TEXT, unsubscribe_url TEXT, unsubscribe_scanned INTEGER NOT NULL DEFAULT 0, is_read INTEGER NOT NULL DEFAULT 0, is_flagged INTEGER NOT NULL DEFAULT 0, has_attachments INTEGER NOT NULL DEFAULT 0, category TEXT, classification_confidence REAL, classification_source TEXT, classification_signals TEXT NOT NULL DEFAULT '', UNIQUE(account_id, mailbox, uid))",
    "CREATE VIRTUAL TABLE messages_fts USING fts5(subject, from_name, from_address, to_addresses, body_text, content='messages', content_rowid='rowid')",
    "CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
    "CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); END",
    "CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_address, to_addresses, body_text) VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_address, old.to_addresses, old.body_text); INSERT INTO messages_fts(rowid, subject, from_name, from_address, to_addresses, body_text) VALUES (new.rowid, new.subject, new.from_name, new.from_address, new.to_addresses, new.body_text); END",
    "CREATE INDEX messages_account_mailbox_date ON messages(account_id, mailbox, received_at DESC)",
    "CREATE TABLE mailbox_sync_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, initialized_at TEXT NOT NULL, highest_uid INTEGER, uid_validity INTEGER, PRIMARY KEY(account_id, mailbox))",
    "CREATE TABLE mailbox_action_tombstones (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
    "CREATE TABLE attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0, data BLOB NOT NULL)",
    "CREATE INDEX attachments_message_id ON attachments(message_id)",
    "CREATE TABLE starred_message_bodies (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, body_text TEXT NOT NULL, body_html TEXT, attachment_presentation_version INTEGER NOT NULL DEFAULT 0, cached_at TEXT NOT NULL)",
    "CREATE TABLE starred_attachment_metadata (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, filename TEXT NOT NULL, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL, is_inline INTEGER NOT NULL DEFAULT 0, presentation TEXT NOT NULL DEFAULT 'unknown', is_potentially_unsafe INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE message_content_cache (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, content_state TEXT NOT NULL CHECK(content_state = 'complete'), body_text TEXT NOT NULL, body_html TEXT, unsubscribe_kind TEXT, attachments_json TEXT NOT NULL, byte_size INTEGER NOT NULL CHECK(byte_size >= 0), last_accessed INTEGER NOT NULL)",
    "CREATE INDEX message_content_cache_lru ON message_content_cache(last_accessed, message_id)",
    "CREATE TABLE message_content_fetches (message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE ON UPDATE CASCADE, claimed_at TEXT NOT NULL, claim_owner TEXT NOT NULL DEFAULT '')",
    "CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE mailbox_catalog_state (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, remote_total INTEGER NOT NULL DEFAULT 0, historical_complete INTEGER NOT NULL DEFAULT 0, uid_next INTEGER, highest_modseq TEXT, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox))",
    "CREATE TABLE mailbox_snapshot_generations (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, generation TEXT NOT NULL, remote_name TEXT NOT NULL, uid_validity INTEGER NOT NULL, initial_exists INTEGER NOT NULL, uid_next INTEGER, highest_modseq TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation))",
    "CREATE TABLE mailbox_snapshot_items (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, is_read INTEGER NOT NULL, is_flagged INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
    "CREATE TABLE mailbox_snapshot_messages (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, message_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
    "CREATE TABLE mailbox_snapshot_replacement_outcomes (account_id TEXT NOT NULL, mailbox TEXT NOT NULL, generation TEXT NOT NULL, uid INTEGER NOT NULL, outcome TEXT NOT NULL CHECK(outcome IN ('message', 'excluded')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, generation, uid), FOREIGN KEY(account_id, mailbox, generation) REFERENCES mailbox_snapshot_generations(account_id, mailbox, generation) ON DELETE CASCADE)",
    "CREATE INDEX mailbox_snapshot_items_generation_uid ON mailbox_snapshot_items(account_id, mailbox, generation, uid DESC)",
    "CREATE TABLE mailbox_sync_failures (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, mailbox TEXT NOT NULL, uid INTEGER NOT NULL, stage TEXT NOT NULL, error TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, mailbox, uid))",
    "CREATE TABLE mail_rebuild_jobs (account_id TEXT PRIMARY KEY, phase TEXT NOT NULL, completed INTEGER NOT NULL DEFAULT 0, total INTEGER, reset_before_sync INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
    "CREATE TABLE deleted_account_tombstones (account_id TEXT PRIMARY KEY, deleted_at TEXT NOT NULL)",
    "CREATE TABLE sent_correspondents (account_id TEXT NOT NULL, address TEXT NOT NULL COLLATE NOCASE, PRIMARY KEY(account_id, address))",
    "ALTER TABLE messages ADD COLUMN body_html TEXT",
    "ALTER TABLE messages ADD COLUMN content_state TEXT NOT NULL DEFAULT 'complete'",
];

#[tokio::test]
async fn sqlite_page_exhaustion_preserves_committed_rows_after_reopen() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("page-limit.sqlite");
    let store = Store::open(&path).await.expect("open store");
    let account_id = Uuid::new_v4();
    store
        .save_account(&account(account_id))
        .await
        .expect("save account");
    let original = message(account_id, 1, "Committed", "must survive");
    store
        .upsert_messages(std::slice::from_ref(&original))
        .await
        .expect("seed committed row");
    store
        .set_secret("fault-secret", "preserve this credential")
        .await
        .expect("seed encrypted credential");
    let attempt_id = Uuid::new_v4();
    assert!(matches!(
        store
            .claim_send_attempt(account_id, attempt_id, "pending-payload")
            .await
            .expect("seed pending send attempt"),
        SendAttemptClaim::Claimed
    ));

    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&store.pool)
        .await
        .expect("read page count");
    let max_page_count: i64 = sqlx::query_scalar(&format!("PRAGMA max_page_count = {page_count}"))
        .fetch_one(&store.pool)
        .await
        .expect("set page limit");
    assert_eq!(max_page_count, page_count);

    let oversized = message(account_id, 2, "Rejected", &"x".repeat(512 * 1024));
    let error = store
        .upsert_messages(&[oversized])
        .await
        .expect_err("the bounded database must reject a page-growing write");
    assert!(
        format!("{error:#}").to_ascii_lowercase().contains("full"),
        "{error:#}"
    );
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen after SQLITE_FULL");
    let preserved = reopened
        .message(&original.id)
        .await
        .expect("read committed row")
        .expect("committed row remains");
    assert_eq!(preserved.subject, original.subject);
    assert_eq!(preserved.account_id, original.account_id);
    assert_eq!(preserved.uid, original.uid);
    assert_eq!(
        reopened
            .secret("fault-secret")
            .await
            .expect("read credential")
            .as_deref(),
        Some("preserve this credential")
    );
    assert!(matches!(
        reopened
            .claim_send_attempt(account_id, attempt_id, "pending-payload")
            .await
            .expect("read pending send attempt"),
        SendAttemptClaim::Existing(SendAttemptStatus::Pending)
    ));
    assert!(reopened
        .message(&format!("{account_id}:INBOX:2"))
        .await
        .expect("check rejected row")
        .is_none());
}

#[tokio::test]
async fn sqlite_full_during_recovery_prepare_reopens_without_a_payload_or_claim_half() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("recovery-full.sqlite");
    let store = Store::open(&path).await.expect("open store");
    let account_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    store
        .save_account(&account(account_id))
        .await
        .expect("save recovery account");

    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&store.pool)
        .await
        .expect("read page count");
    let max_page_count: i64 = sqlx::query_scalar(&format!("PRAGMA max_page_count = {page_count}"))
        .fetch_one(&store.pool)
        .await
        .expect("set page limit");
    assert_eq!(max_page_count, page_count);

    let error = store
        .prepare_send_recovery(
            account_id,
            attempt_id,
            &recovery_message(account_id, "x".repeat(512 * 1024)),
        )
        .await
        .expect_err("the bounded database must reject the recovery payload write");
    assert!(
        format!("{error:#}").to_ascii_lowercase().contains("full"),
        "{error:#}"
    );
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen after SQLITE_FULL");
    assert!(reopened
        .load_send_recovery(account_id, attempt_id)
        .await
        .expect("read failed recovery attempt")
        .is_none());
    let recovery_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(account_id.to_string())
    .bind(attempt_id.to_string())
    .fetch_one(&reopened.pool)
    .await
    .expect("count recovery rows after SQLITE_FULL");
    let journal_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(account_id.to_string())
    .bind(attempt_id.to_string())
    .fetch_one(&reopened.pool)
    .await
    .expect("count journal rows after SQLITE_FULL");
    assert_eq!(recovery_rows, 0, "SQLITE_FULL left a recovery payload half");
    assert_eq!(journal_rows, 0, "SQLITE_FULL left a journal claim half");
}

#[tokio::test]
async fn busy_recovery_prepare_waits_for_the_lock_then_reopens_as_a_matched_pair() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("recovery-busy.sqlite");
    let store = Store::open(&path).await.expect("open store");
    let account_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    store
        .save_account(&account(account_id))
        .await
        .expect("save recovery account");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .foreign_keys(true);
    let mut lock = <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&options)
        .await
        .expect("open competing writer");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut lock)
        .await
        .expect("hold competing write lock");

    let message = recovery_message(account_id, "Lock-safe recovery payload".into());
    let mut prepare = tokio::spawn({
        let store = store.clone();
        async move {
            store
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut prepare)
            .await
            .is_err(),
        "prepare must not bypass an active SQLite writer lock"
    );
    sqlx::query("COMMIT")
        .execute(&mut lock)
        .await
        .expect("release competing write lock");
    <sqlx::SqliteConnection as sqlx::Connection>::close(lock)
        .await
        .expect("close competing writer");
    assert!(matches!(
        prepare
            .await
            .expect("join recovery prepare")
            .expect("prepare after lock"),
        crate::SendRecoveryClaim::Claimed
    ));
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen after writer lock");
    let recovery = reopened
        .load_send_recovery(account_id, attempt_id)
        .await
        .expect("read recovery after writer lock")
        .expect("recovery is durable after writer lock");
    assert_eq!(
        recovery.status,
        crate::SendRecoveryStatus::Pending,
        "busy retry must not release a pending send"
    );
    let pairs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM send_recovery_payloads r JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id = ? AND r.attempt_id = ? AND r.retryable = 0 AND j.status = 'pending' AND r.payload_hash = j.payload_hash",
    )
    .bind(account_id.to_string())
    .bind(attempt_id.to_string())
    .fetch_one(&reopened.pool)
    .await
    .expect("count matched recovery/journal pairs");
    assert_eq!(pairs, 1, "busy recovery prepare left an incomplete pair");
}

#[cfg(unix)]
#[tokio::test]
async fn readonly_database_refuses_migration_without_replacing_source() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("readonly.sqlite");
    let store = Store::open(&path).await.expect("initial open");
    let account_id = Uuid::new_v4();
    store
        .save_account(&account(account_id))
        .await
        .expect("save account");
    let original = message(account_id, 1, "Read only", "preserve this");
    store
        .upsert_messages(std::slice::from_ref(&original))
        .await
        .expect("seed row");
    drop(store);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("make database readonly");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o555))
        .expect("make database directory readonly");

    let error = match Store::open(&path).await {
        Ok(_) => panic!("readonly storage must reject migration writes"),
        Err(error) => error,
    };
    assert!(
        !error.to_string().is_empty(),
        "readonly open must explain failure"
    );
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("restore directory permissions");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("restore permissions");

    let reopened = Store::open(&path)
        .await
        .expect("reopen after readonly failure");
    let preserved = reopened
        .message(&original.id)
        .await
        .expect("read preserved message")
        .expect("readonly failure must not lose committed data");
    assert_eq!(preserved.subject, original.subject);
    assert_eq!(preserved.account_id, original.account_id);
    assert_eq!(preserved.uid, original.uid);
}

#[tokio::test]
async fn corrupt_database_is_rejected_without_replacing_source() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("corrupt.sqlite");
    let bytes = b"Dakia corrupt database fixture\0\xff\x01";
    fs::write(&path, bytes).expect("write corrupt fixture");

    let error = match Store::open(&path).await {
        Ok(_) => panic!("corrupt storage must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().to_ascii_lowercase().contains("database"),
        "{error:#}"
    );
    assert_eq!(fs::read(&path).expect("read corrupt fixture"), bytes);
    assert!(!profile_migration_lock_path(&path)
        .expect("corrupt profile lock path")
        .exists());
}

#[tokio::test]
async fn migration_process_termination_preserves_source_and_reopens_to_completion() {
    const CHILD_DB_ENV: &str = "DAKIA_TEST_MIGRATION_CHILD_DB";
    const ABORT_ENV: &str = "DAKIA_TEST_ABORT_MIGRATION_AFTER_BASE_SCHEMA";
    if let Some(path) = std::env::var_os(CHILD_DB_ENV) {
        let _ = Store::open(path).await;
        panic!("migration checkpoint child should have terminated inside Store::open");
    }

    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("interrupted-migration.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse fixture path")
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("create published profile fixture");
    for statement in V0_4_4_SCHEMA {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("v0.4.4 schema statement failed: {statement}: {error}"));
    }
    let account_id = Uuid::new_v4();
    let fixture_account = account(account_id);
    let fixture_message = message(account_id, 5, "Interrupted migration", "preserve source");
    sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?)")
        .bind(account_id.to_string())
        .bind(&fixture_account.email)
        .bind(serde_json::to_string(&fixture_account).expect("serialize account"))
        .bind(fixture_account.created_at)
        .execute(&pool)
        .await
        .expect("seed account");
    sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, thread_id, subject, from_address, to_addresses, received_at, snippet, body_text, content_state) VALUES (?, ?, 'INBOX', 5, ?, 'interrupted-thread', ?, 'sender@example.test', 'recipient@example.test', ?, 'preserve source', 'preserve source', 'complete')")
        .bind(&fixture_message.id)
        .bind(&fixture_message.account_id)
        .bind(&fixture_message.message_id)
        .bind(&fixture_message.subject)
        .bind(fixture_message.received_at)
        .execute(&pool)
        .await
        .expect("seed message");
    for (key, value) in [
        ("classification_policy_version", "2"),
        ("attachment_presentation_cache_version", "3"),
        ("catalogue_schema", "1"),
        ("sent_correspondents_version", "1"),
    ] {
        sqlx::query("INSERT INTO app_meta(key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .expect("seed release marker");
    }
    pool.close().await;
    let key: [u8; VAULT_KEY_LEN] = random_bytes().expect("generate fictional vault key");
    fs::write(directory.path().join(VAULT_KEY_FILE), key).expect("write fixture vault key");

    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("storage::storage_fault_tests::migration_process_termination_preserves_source_and_reopens_to_completion")
        .arg("--nocapture")
        .env(CHILD_DB_ENV, &path)
        .env(ABORT_ENV, "1")
        .output()
        .expect("launch isolated migration child");
    assert_eq!(
        output.status.code(),
        Some(86),
        "migration child did not terminate at the checkpoint: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let inspection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse interrupted profile path")
            .foreign_keys(true),
    )
    .await
    .expect("inspect interrupted profile");
    for table in ["accounts", "messages"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&inspection)
            .await
            .expect("count source rows after termination");
        assert_eq!(count, 1, "{table} source row was lost");
    }
    let send_attempts_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'send_attempts')",
    )
    .fetch_one(&inspection)
    .await
    .expect("inspect checkpoint progress");
    assert!(
        send_attempts_exists,
        "child did not reach the migration checkpoint"
    );
    let completed_marker: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&inspection)
            .await
            .expect("inspect incomplete schema marker");
    assert_eq!(completed_marker, 0, "interrupted migration looked complete");
    inspection.close().await;

    let reopened = Store::open(&path)
        .await
        .expect("restart completes the interrupted migration");
    assert_eq!(
        reopened
            .account(account_id)
            .await
            .expect("read account")
            .expect("account survives")
            .email,
        fixture_account.email
    );
    assert_eq!(
        reopened
            .message(&fixture_message.id)
            .await
            .expect("read message")
            .expect("message survives")
            .subject,
        "Interrupted migration"
    );
    let completed_version: String =
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&reopened.pool)
            .await
            .expect("read completed schema marker");
    assert_eq!(completed_version, "3");
}

#[tokio::test]
async fn malformed_account_row_reports_an_error_without_hiding_other_recoverable_state() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("malformed-row.sqlite");
    let store = Store::open(&path).await.expect("open profile");
    let account_id = Uuid::new_v4();
    let valid_account = account(account_id);
    let valid_message = message(account_id, 1, "Recoverable", "durable state");
    store
        .save_account(&valid_account)
        .await
        .expect("save valid account");
    store
        .upsert_messages(std::slice::from_ref(&valid_message))
        .await
        .expect("save valid message");
    store
        .set_secret("malformed-row-secret", "recoverable credential")
        .await
        .expect("save valid credential");
    let malformed_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts(id, email, data, created_at) VALUES (?, 'malformed@example.test', '{', ?)",
    )
    .bind(malformed_id.to_string())
    .bind(Utc::now())
    .execute(&store.pool)
    .await
    .expect("inject one malformed serialized account row");
    store.pool.close().await;

    let reopened = Store::open(&path)
        .await
        .expect("a malformed logical row must not make the SQLite profile unreadable");
    let list_error = reopened
        .accounts()
        .await
        .expect_err("the malformed row must be reported instead of silently omitted");
    let list_error_chain = format!("{list_error:#}").to_ascii_lowercase();
    assert!(
        list_error_chain.contains("json")
            || list_error_chain.contains("expected")
            || list_error_chain.contains("eof"),
        "{list_error:#}"
    );
    assert_eq!(
        reopened
            .account(account_id)
            .await
            .expect("read valid account directly")
            .expect("valid account remains")
            .email,
        valid_account.email
    );
    assert_eq!(
        reopened
            .message(&valid_message.id)
            .await
            .expect("read valid message")
            .expect("valid message remains")
            .subject,
        "Recoverable"
    );
    assert_eq!(
        reopened
            .secret("malformed-row-secret")
            .await
            .expect("read valid credential")
            .as_deref(),
        Some("recoverable credential")
    );
    let malformed_data: String = sqlx::query_scalar("SELECT data FROM accounts WHERE id = ?")
        .bind(malformed_id.to_string())
        .fetch_one(&reopened.pool)
        .await
        .expect("malformed source row remains available for recovery");
    assert_eq!(malformed_data, "{");
}

#[tokio::test]
async fn published_v0_4_4_profile_upgrades_without_losing_user_state() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("v0.4.4-profile.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse fixture path")
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("create v0.4.4 profile");
    for statement in V0_4_4_SCHEMA {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("v0.4.4 schema statement failed: {statement}: {error}"));
    }

    let account_id = Uuid::new_v4();
    let fixture_account = account(account_id);
    let fixture_message = message(account_id, 44, "v0.4.4 message", "preserve body");
    sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, ?, ?)")
        .bind(account_id.to_string())
        .bind(&fixture_account.email)
        .bind(serde_json::to_string(&fixture_account).expect("serialize fixture account"))
        .bind(fixture_account.created_at)
        .execute(&pool)
        .await
        .expect("seed v0.4.4 account");
    sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, threading_scanned, recipient_headers_scanned, subject, from_name, from_address, to_addresses, cc_addresses, bcc_addresses, reply_to_addresses, received_at, snippet, body_text, body_html, content_state, unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, is_flagged, has_attachments, category, classification_confidence, classification_source, classification_signals) VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 1, 1, ?, ?, ?, ?, '', '', '', ?, ?, ?, NULL, 'complete', NULL, NULL, 1, 0, 1, 0, NULL, NULL, NULL, '')")
        .bind(&fixture_message.id)
        .bind(&fixture_message.account_id)
        .bind(&fixture_message.mailbox)
        .bind(fixture_message.uid)
        .bind(&fixture_message.message_id)
        .bind(&fixture_message.thread_id)
        .bind(&fixture_message.subject)
        .bind(&fixture_message.from_name)
        .bind(&fixture_message.from_address)
        .bind(&fixture_message.to_addresses)
        .bind(fixture_message.received_at)
        .bind(&fixture_message.snippet)
        .bind(&fixture_message.body_text)
        .execute(&pool)
        .await
        .expect("seed v0.4.4 message");
    sqlx::query("INSERT INTO mail_rebuild_jobs(account_id, phase, completed, total, reset_before_sync, updated_at) VALUES (?, 'historical-rebuild', 4, 12, 1, ?)")
        .bind(account_id.to_string())
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("seed v0.4.4 rebuild state");
    sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'INBOX', 91, ?)")
        .bind(account_id.to_string())
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("seed v0.4.4 mailbox action");
    for (key, value) in [
        ("classification_policy_version", "2"),
        ("attachment_presentation_cache_version", "3"),
        ("catalogue_schema", "1"),
        ("sent_correspondents_version", "1"),
    ] {
        sqlx::query("INSERT INTO app_meta(key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .expect("seed v0.4.4 migration marker");
    }

    let key: [u8; VAULT_KEY_LEN] = random_bytes().expect("generate fictional vault key");
    let nonce: [u8; VAULT_NONCE_LEN] = random_bytes().expect("generate fictional nonce");
    let secret_name = "v0.4.4-fictional-secret";
    let ciphertext = encrypt_secret(&key, nonce, secret_name, "fictional credential")
        .expect("encrypt fictional credential");
    fs::write(directory.path().join(VAULT_KEY_FILE), key).expect("write fixture vault key");
    sqlx::query("INSERT INTO credentials(name, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?)")
        .bind(secret_name)
        .bind(nonce.to_vec())
        .bind(ciphertext)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("seed v0.4.4 credential");
    pool.close().await;

    let upgraded = Store::open(&path).await.expect("upgrade v0.4.4 profile");
    assert_eq!(
        upgraded
            .account(account_id)
            .await
            .expect("read account")
            .expect("account survives")
            .email,
        fixture_account.email
    );
    let restored_message = upgraded
        .message(&fixture_message.id)
        .await
        .expect("read message")
        .expect("message survives");
    assert_eq!(restored_message.subject, "v0.4.4 message");
    assert_eq!(restored_message.body_text, "preserve body");
    assert!(restored_message.is_flagged);
    assert_eq!(
        upgraded
            .secret(secret_name)
            .await
            .expect("decrypt credential")
            .as_deref(),
        Some("fictional credential")
    );
    let jobs = upgraded
        .mail_rebuild_jobs()
        .await
        .expect("read rebuild state");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].phase, "historical-rebuild");
    assert_eq!(jobs[0].completed, 4);
    assert_eq!(jobs[0].total, Some(12));
    assert!(jobs[0].reset_before_sync);
    let tombstones: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mailbox_action_tombstones WHERE account_id = ? AND mailbox = 'INBOX' AND uid = 91")
        .bind(account_id.to_string())
        .fetch_one(&upgraded.pool)
        .await
        .expect("read mailbox action state");
    assert_eq!(tombstones, 1);
    let version: String =
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&upgraded.pool)
            .await
            .expect("read candidate schema marker");
    assert_eq!(version, "3");
    let provider_identity_table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'message_provider_identities')",
    )
    .fetch_one(&upgraded.pool)
    .await
    .expect("read Gmail provider identity migration");
    assert!(provider_identity_table_exists);
    let send_attempts_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'send_attempts')",
    )
    .fetch_one(&upgraded.pool)
    .await
    .expect("read candidate send journal schema");
    assert!(send_attempts_exists);
    let send_recovery_payloads_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'send_recovery_payloads')",
    )
    .fetch_one(&upgraded.pool)
    .await
    .expect("read candidate send recovery schema");
    assert!(send_recovery_payloads_exists);
    let recovery_payload_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM send_recovery_payloads")
            .fetch_one(&upgraded.pool)
            .await
            .expect("read migrated send recovery payloads");
    assert_eq!(recovery_payload_count, 0);
}

#[tokio::test]
async fn newer_profile_schema_fails_before_migration_and_remains_recoverable() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("future-profile.sqlite");
    let store = Store::open(&path).await.expect("open current profile");
    let account_id = Uuid::new_v4();
    let fixture_account = account(account_id);
    store
        .save_account(&fixture_account)
        .await
        .expect("save fictional account");
    let fixture_message = message(account_id, 7, "Future profile", "preserve message");
    store
        .upsert_messages(std::slice::from_ref(&fixture_message))
        .await
        .expect("save fictional message");
    store
        .set_secret("future-profile-secret", "preserve credential")
        .await
        .expect("save fictional credential");
    let attempt_id = Uuid::new_v4();
    let recovery = recovery_message(account_id, "preserve recovery bytes".into());
    let recovery_bytes = serde_json::to_vec(&recovery).expect("serialize exact recovery fixture");
    let recovery_hash = format!("{:x}", Sha256::digest(&recovery_bytes));
    assert!(matches!(
        store
            .prepare_send_recovery(account_id, attempt_id, &recovery)
            .await
            .expect("save pending recovery action"),
        SendRecoveryClaim::Claimed
    ));
    store
        .save_mail_rebuild_job(&MailRebuildJob {
            account_id,
            phase: "preserve-rebuild".into(),
            completed: 3,
            total: Some(9),
            reset_before_sync: true,
        })
        .await
        .expect("save rebuild state");
    sqlx::query("UPDATE app_meta SET value = '4' WHERE key = 'profile_schema_version'")
        .execute(&store.pool)
        .await
        .expect("simulate a profile created by a newer Dakia release");
    store.pool.close().await;

    let error = match Store::open(&path).await {
        Ok(_) => panic!("an older build must refuse a newer profile schema"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema version 4 is newer"),
        "{error:#}"
    );

    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse fixture path")
        .foreign_keys(true);
    let inspection = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("inspect refused profile");
    for (table, expected) in [
        ("accounts", 1_i64),
        ("messages", 1),
        ("credentials", 1),
        ("send_attempts", 1),
        ("send_recovery_payloads", 1),
        ("mail_rebuild_jobs", 1),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&inspection)
            .await
            .expect("count preserved profile rows");
        assert_eq!(count, expected, "{table} changed during refused open");
    }
    let (stored_payload, retryable, stored_recovery_hash, journal_payload_hash, journal_status):
        (Vec<u8>, i64, String, String, String) = sqlx::query_as(
        "SELECT r.payload_json, r.retryable, r.payload_hash, j.payload_hash, j.status FROM send_recovery_payloads r JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id = ? AND r.attempt_id = ?",
    )
    .bind(account_id.to_string())
    .bind(attempt_id.to_string())
    .fetch_one(&inspection)
    .await
    .expect("read preserved recovery and journal rows");
    assert_eq!(
        stored_payload, recovery_bytes,
        "refused open changed recovery bytes"
    );
    assert_eq!(retryable, 0, "refused open changed recovery retryability");
    assert_eq!(
        stored_recovery_hash, recovery_hash,
        "refused open changed recovery payload hash"
    );
    assert_eq!(
        journal_payload_hash, recovery_hash,
        "refused open changed journal payload hash"
    );
    assert_eq!(
        journal_status, "pending",
        "refused open changed journal status"
    );
    sqlx::query("UPDATE app_meta SET value = '3' WHERE key = 'profile_schema_version'")
        .execute(&inspection)
        .await
        .expect("restore supported version marker");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&inspection)
        .await
        .expect("checkpoint repaired version marker");
    inspection.close().await;

    let reopened = Store::open(&path)
        .await
        .expect("supported profile reopens after refusal");
    assert_eq!(
        reopened
            .account(account_id)
            .await
            .expect("read account")
            .expect("account remains")
            .email,
        fixture_account.email
    );
    assert_eq!(
        reopened
            .message(&fixture_message.id)
            .await
            .expect("read message")
            .expect("message remains")
            .subject,
        "Future profile"
    );
    assert_eq!(
        reopened
            .secret("future-profile-secret")
            .await
            .expect("read credential")
            .as_deref(),
        Some("preserve credential")
    );
    let (reopened_payload, reopened_retryable, reopened_recovery_hash, reopened_journal_hash, reopened_journal_status):
        (Vec<u8>, i64, String, String, String) = sqlx::query_as(
        "SELECT r.payload_json, r.retryable, r.payload_hash, j.payload_hash, j.status FROM send_recovery_payloads r JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id = ? AND r.attempt_id = ?",
    )
    .bind(account_id.to_string())
    .bind(attempt_id.to_string())
    .fetch_one(&reopened.pool)
    .await
    .expect("read reopened recovery and journal rows");
    assert_eq!(
        reopened_payload, recovery_bytes,
        "reopen changed recovery bytes"
    );
    assert_eq!(
        reopened_retryable, 0,
        "reopen changed recovery retryability"
    );
    assert_eq!(
        reopened_recovery_hash, recovery_hash,
        "reopen changed recovery payload hash"
    );
    assert_eq!(
        reopened_journal_hash, recovery_hash,
        "reopen changed journal payload hash"
    );
    assert_eq!(
        reopened_journal_status, "pending",
        "reopen changed journal status"
    );
    let restored_recovery = reopened
        .load_send_recovery(account_id, attempt_id)
        .await
        .expect("read preserved recovery")
        .expect("recovery remains after refused open");
    assert_eq!(restored_recovery.status, SendRecoveryStatus::Pending);
    assert_eq!(
        serde_json::to_vec(&restored_recovery.message).expect("serialize restored recovery"),
        recovery_bytes,
        "reopened recovery bytes changed"
    );
    let jobs = reopened
        .mail_rebuild_jobs()
        .await
        .expect("read rebuild state");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].phase, "preserve-rebuild");
    assert_eq!(jobs[0].completed, 3);
    assert_eq!(jobs[0].total, Some(9));
    assert!(jobs[0].reset_before_sync);
}

#[tokio::test]
async fn future_profile_is_refused_before_vault_or_wal_side_effects() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("future-readonly-preflight.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse fixture path")
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("create future profile fixture");
    sqlx::query("CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&pool)
        .await
        .expect("create version table");
    sqlx::query("INSERT INTO app_meta(key, value) VALUES ('profile_schema_version', '4')")
        .execute(&pool)
        .await
        .expect("seed future version");
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .expect("read original journal mode");
    assert_eq!(journal_mode.to_ascii_lowercase(), "delete");
    pool.close().await;

    let database_before = fs::read(&path).expect("read future database before refusal");
    let mut entries_before = fs::read_dir(directory.path())
        .expect("list fixture directory")
        .map(|entry| entry.expect("read fixture entry").file_name())
        .collect::<Vec<_>>();
    entries_before.sort();
    assert!(!directory.path().join(VAULT_KEY_FILE).exists());

    let error = match Store::open(&path).await {
        Ok(_) => panic!("future profile must be refused by readonly preflight"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema version 4 is newer"),
        "{error:#}"
    );
    assert!(!directory.path().join(VAULT_KEY_FILE).exists());
    assert_eq!(
        fs::read(&path).expect("read future database after refusal"),
        database_before
    );
    assert!(!profile_migration_lock_path(&path)
        .expect("future profile lock path")
        .exists());
    let mut entries_after = fs::read_dir(directory.path())
        .expect("list refused profile directory")
        .map(|entry| entry.expect("read refused entry").file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(entries_after, entries_before);

    let inspection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse refused profile path")
            .read_only(true)
            .immutable(true),
    )
    .await
    .expect("inspect refused profile");
    let journal_mode_after: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&inspection)
        .await
        .expect("read journal mode after refusal");
    assert_eq!(journal_mode_after.to_ascii_lowercase(), "delete");
}

#[tokio::test]
async fn interrupted_future_migration_target_is_refused_even_without_future_completion_marker() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("interrupted-future-migration.sqlite");
    let store = Store::open(&path).await.expect("open current profile");
    let account_id = Uuid::new_v4();
    store
        .save_account(&account(account_id))
        .await
        .expect("seed recoverable account");
    sqlx::query("INSERT INTO app_meta(key, value) VALUES ('profile_schema_migration_target', '4')")
        .execute(&store.pool)
        .await
        .expect("simulate interrupted migration from a future build");
    store.pool.close().await;

    let error = match Store::open(&path).await {
        Ok(_) => panic!("older build must refuse an interrupted future migration"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema version 4 is newer"),
        "{error:#}"
    );
    let inspection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse refused profile path")
            .read_only(true),
    )
    .await
    .expect("inspect refused future migration");
    let preserved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = ?")
        .bind(account_id.to_string())
        .fetch_one(&inspection)
        .await
        .expect("count preserved account");
    assert_eq!(preserved, 1);
    let completed: String =
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&inspection)
            .await
            .expect("read unchanged completion marker");
    assert_eq!(completed, "3");
}

#[tokio::test]
async fn version_preflight_reads_live_wal_instead_of_stale_main_database() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("live-wal-version.sqlite");
    let base = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse fixture path")
            .create_if_missing(true),
    )
    .await
    .expect("create base profile");
    sqlx::query("CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&base)
        .await
        .expect("create version table");
    sqlx::query("CREATE TABLE wal_evidence (value TEXT NOT NULL)")
        .execute(&base)
        .await
        .expect("create WAL evidence table");
    sqlx::query("INSERT INTO app_meta VALUES ('profile_schema_version', '4')")
        .execute(&base)
        .await
        .expect("seed stale future version");
    sqlx::query("INSERT INTO wal_evidence VALUES ('stale-main')")
        .execute(&base)
        .await
        .expect("seed stale evidence");
    base.close().await;

    let writer = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse WAL fixture path")
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
    )
    .await
    .expect("open live WAL writer");
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&writer)
        .await
        .expect("disable automatic checkpoint");
    sqlx::query("UPDATE app_meta SET value = '3' WHERE key = 'profile_schema_version'")
        .execute(&writer)
        .await
        .expect("write supported version to live WAL");
    sqlx::query("UPDATE wal_evidence SET value = 'current-wal'")
        .execute(&writer)
        .await
        .expect("write evidence to live WAL");
    assert!(path.with_extension("sqlite-wal").exists());
    assert!(path.with_extension("sqlite-shm").exists());

    let reopened = Store::open(&path)
        .await
        .expect("preflight must see the supported version in the live WAL");
    let evidence: String = sqlx::query_scalar("SELECT value FROM wal_evidence")
        .fetch_one(&reopened.pool)
        .await
        .expect("read current WAL evidence");
    assert_eq!(evidence, "current-wal");
    writer.close().await;
}

#[tokio::test]
async fn valid_wal_without_shared_memory_recovers_instead_of_trusting_the_stale_main_database() {
    let source_directory = tempdir().expect("temporary WAL source directory");
    let source_path = source_directory.path().join("source.sqlite");
    let base = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(source_path.to_str().unwrap())
            .expect("parse WAL source path")
            .create_if_missing(true),
    )
    .await
    .expect("create WAL source profile");
    sqlx::query("CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&base)
        .await
        .expect("create version table");
    sqlx::query("INSERT INTO app_meta VALUES ('profile_schema_version', '4')")
        .execute(&base)
        .await
        .expect("seed future version in the main database");
    base.close().await;

    let writer = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(source_path.to_str().unwrap())
            .expect("parse WAL writer path")
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
    )
    .await
    .expect("open WAL writer");
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&writer)
        .await
        .expect("disable automatic checkpoint");
    sqlx::query("UPDATE app_meta SET value = '3' WHERE key = 'profile_schema_version'")
        .execute(&writer)
        .await
        .expect("write current version only to WAL");
    let source_wal = sqlite_sidecar_path(&source_path, "-wal");
    let source_shm = sqlite_sidecar_path(&source_path, "-shm");
    assert!(source_wal.exists());
    assert!(source_shm.exists());

    let recovered_directory = tempdir().expect("temporary recovered profile directory");
    let recovered_path = recovered_directory.path().join("wal-without-shm.sqlite");
    fs::copy(&source_path, &recovered_path).expect("copy main database");
    fs::copy(&source_wal, sqlite_sidecar_path(&recovered_path, "-wal"))
        .expect("copy live WAL without shared memory");
    assert!(!sqlite_sidecar_path(&recovered_path, "-shm").exists());

    let recovered = Store::open(&recovered_path)
        .await
        .expect("SQLite must rebuild transient SHM and read the current version from WAL");
    let version: String =
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&recovered.pool)
            .await
            .expect("read recovered profile version");
    assert_eq!(version, "3");

    writer.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_schema_in_wal_without_shared_memory_preserves_durable_profile_through_symlink() {
    use std::os::unix::fs::symlink;

    let source_directory = tempdir().expect("temporary WAL source directory");
    let source_path = source_directory.path().join("source.sqlite");
    let store = Store::open(&source_path)
        .await
        .expect("create current profile");
    let account_id = Uuid::new_v4();
    let fixture_account = account(account_id);
    let fixture_message = message(account_id, 81, "WAL conflict", "preserve durable mail");
    store
        .save_account(&fixture_account)
        .await
        .expect("save fictional account");
    store
        .upsert_messages(std::slice::from_ref(&fixture_message))
        .await
        .expect("save fictional message");
    store
        .set_secret("wal-conflict-secret", "preserve fictional credential")
        .await
        .expect("save fictional credential");
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&store.pool)
        .await
        .expect("disable automatic checkpoint");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&store.pool)
        .await
        .expect("checkpoint durable profile state");
    sqlx::query("DROP TABLE send_attempts")
        .execute(&store.pool)
        .await
        .expect("remove valid send journal in WAL");
    sqlx::query("CREATE VIEW send_attempts AS SELECT 'fictional' AS incompatible")
        .execute(&store.pool)
        .await
        .expect("write conflicting schema only to WAL");
    let source_wal = sqlite_sidecar_path(&source_path, "-wal");
    assert!(source_wal.exists());

    let profile_directory = tempdir().expect("temporary aliased profile directory");
    let target_directory = profile_directory.path().join("target");
    fs::create_dir(&target_directory).expect("create symlink target directory");
    let target_path = target_directory.join("profile.sqlite");
    let target_wal = sqlite_sidecar_path(&target_path, "-wal");
    let target_vault = target_directory.join(VAULT_KEY_FILE);
    fs::copy(&source_path, &target_path).expect("copy durable database");
    fs::copy(&source_wal, &target_wal).expect("copy conflicting WAL");
    fs::copy(source_directory.path().join(VAULT_KEY_FILE), &target_vault)
        .expect("copy fictional vault");
    let alias_path = profile_directory.path().join("profile-alias.sqlite");
    symlink(&target_path, &alias_path).expect("create database symlink");
    assert!(!sqlite_sidecar_path(&target_path, "-shm").exists());

    let database_before = fs::read(&target_path).expect("read database before refusal");
    let wal_before = fs::read(&target_wal).expect("read WAL before refusal");
    let vault_before = fs::read(&target_vault).expect("read vault before refusal");

    let error = match Store::open(&alias_path).await {
        Ok(_) => panic!("schema conflict in live WAL must be refused"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema validation failed"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(&target_path).expect("read database after refusal"),
        database_before
    );
    assert_eq!(
        fs::read(&target_wal).expect("read WAL after refusal"),
        wal_before
    );
    assert_eq!(
        fs::read(&target_vault).expect("read vault after refusal"),
        vault_before
    );
    assert!(!profile_directory.path().join(VAULT_KEY_FILE).exists());

    let inspection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(target_path.to_str().unwrap())
            .expect("parse refused target path")
            .read_only(true),
    )
    .await
    .expect("inspect refused WAL profile");
    let migration_marker_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_meta WHERE key = 'profile_schema_migration_target'",
    )
    .fetch_one(&inspection)
    .await
    .expect("inspect migration marker");
    assert_eq!(migration_marker_count, 0);
    let account_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = ?")
        .bind(account_id.to_string())
        .fetch_one(&inspection)
        .await
        .expect("inspect preserved account");
    let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = ?")
        .bind(&fixture_message.id)
        .fetch_one(&inspection)
        .await
        .expect("inspect preserved message");
    assert_eq!(account_count, 1);
    assert_eq!(message_count, 1);
    inspection.close().await;
    store.pool.close().await;
}

async fn assert_conflicting_upgrade_schema_is_rejected_without_profile_writes(
    retain_current_marker: bool,
) {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("conflicting-upgrade.sqlite");
    let store = Store::open(&path).await.expect("open profile");
    let account_id = Uuid::new_v4();
    let fixture_account = account(account_id);
    let fixture_message = message(account_id, 8, "Upgrade conflict", "preserve source");
    store
        .save_account(&fixture_account)
        .await
        .expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&fixture_message))
        .await
        .expect("save message");
    store
        .set_secret("conflicting-upgrade-secret", "preserve credential")
        .await
        .expect("save credential");
    sqlx::query("DROP TABLE send_attempts")
        .execute(&store.pool)
        .await
        .expect("remove candidate table");
    sqlx::query("CREATE TABLE send_attempts (incompatible TEXT NOT NULL)")
        .execute(&store.pool)
        .await
        .expect("inject conflicting upgrade table");
    if !retain_current_marker {
        sqlx::query("DELETE FROM app_meta WHERE key = 'profile_schema_version'")
            .execute(&store.pool)
            .await
            .expect("simulate an incomplete pre-marker upgrade");
    }
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&store.pool)
        .await
        .expect("checkpoint conflicting profile fixture");
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode = DELETE")
        .fetch_one(&store.pool)
        .await
        .expect("stabilize conflicting profile fixture");
    assert_eq!(journal_mode.to_ascii_lowercase(), "delete");
    store.pool.close().await;

    let profile_before = profile_files(directory.path());
    assert!(profile_before
        .iter()
        .any(|(name, _)| name == VAULT_KEY_FILE));
    assert!(!profile_before.iter().any(|(name, _)| {
        name.to_string_lossy().ends_with("-wal") || name.to_string_lossy().ends_with("-shm")
    }));

    let error = match Store::open(&path).await {
        Ok(_) => panic!("conflicting schema must fail validation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema validation failed"),
        "{error:#}"
    );
    assert_eq!(
        profile_files(directory.path()),
        profile_before,
        "read-only schema preflight must preserve every profile file byte"
    );
    let inspection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse profile path")
            .foreign_keys(true),
    )
    .await
    .expect("inspect failed upgrade");
    for table in ["accounts", "messages", "credentials"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&inspection)
            .await
            .expect("count preserved rows");
        assert_eq!(count, 1, "{table} source state was lost");
    }
    let marker_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&inspection)
            .await
            .expect("inspect completion marker");
    assert_eq!(marker_count, i64::from(retain_current_marker));
    let migration_marker_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_meta WHERE key = 'profile_schema_migration_target'",
    )
    .fetch_one(&inspection)
    .await
    .expect("inspect migration marker");
    assert_eq!(migration_marker_count, 0);
    sqlx::query("DROP TABLE send_attempts")
        .execute(&inspection)
        .await
        .expect("remove conflicting table for recovery");
    inspection.close().await;

    let recovered = Store::open(&path)
        .await
        .expect("profile reopens after external conflict repair");
    assert_eq!(
        recovered
            .account(account_id)
            .await
            .expect("read account")
            .expect("account remains")
            .email,
        fixture_account.email
    );
    assert_eq!(
        recovered
            .message(&fixture_message.id)
            .await
            .expect("read message")
            .expect("message remains")
            .subject,
        "Upgrade conflict"
    );
    assert_eq!(
        recovered
            .secret("conflicting-upgrade-secret")
            .await
            .expect("read credential")
            .as_deref(),
        Some("preserve credential")
    );
}

#[tokio::test]
async fn conflicting_upgrade_schema_fails_without_marking_or_deleting_source_state() {
    assert_conflicting_upgrade_schema_is_rejected_without_profile_writes(false).await;
}

#[tokio::test]
async fn current_profile_with_conflicting_schema_is_rejected_without_any_profile_write() {
    assert_conflicting_upgrade_schema_is_rejected_without_profile_writes(true).await;
}

async fn assert_schema_object_conflict_is_rejected_before_vault_creation(
    fixture_name: &str,
    conflicting_object: &str,
) {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join(fixture_name);
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .expect("parse fixture path")
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("create conflicting profile fixture");
    sqlx::query("CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&pool)
        .await
        .expect("create profile metadata table");
    sqlx::query("INSERT INTO app_meta VALUES ('profile_schema_version', '1')")
        .execute(&pool)
        .await
        .expect("mark fixture as a current profile");
    sqlx::query(conflicting_object)
        .execute(&pool)
        .await
        .expect("inject conflicting schema object");
    pool.close().await;

    let profile_before = profile_files(directory.path());
    assert!(!directory.path().join(VAULT_KEY_FILE).exists());

    let error = match Store::open(&path).await {
        Ok(_) => panic!("conflicting schema must fail read-only validation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("profile schema validation failed"),
        "{error:#}"
    );
    assert_eq!(profile_files(directory.path()), profile_before);
    assert!(!directory.path().join(VAULT_KEY_FILE).exists());
}

#[tokio::test]
async fn conflicting_schema_is_rejected_before_creating_a_missing_vault_key() {
    assert_schema_object_conflict_is_rejected_before_vault_creation(
        "conflicting-schema-without-vault.sqlite",
        "CREATE TABLE send_attempts (incompatible TEXT NOT NULL)",
    )
    .await;
}

#[tokio::test]
async fn uppercase_conflicting_table_is_rejected_before_any_profile_write() {
    assert_schema_object_conflict_is_rejected_before_vault_creation(
        "uppercase-conflicting-schema.sqlite",
        "CREATE TABLE SEND_ATTEMPTS (incompatible TEXT NOT NULL)",
    )
    .await;
}

#[tokio::test]
async fn conflicting_view_is_rejected_before_any_profile_write() {
    assert_schema_object_conflict_is_rejected_before_vault_creation(
        "conflicting-schema-view.sqlite",
        "CREATE VIEW send_attempts AS SELECT 'fictional' AS incompatible",
    )
    .await;
}

#[tokio::test]
async fn conflicting_index_is_rejected_before_any_profile_write() {
    assert_schema_object_conflict_is_rejected_before_vault_creation(
        "conflicting-schema-index.sqlite",
        "CREATE INDEX send_attempts ON app_meta(value)",
    )
    .await;
}

#[tokio::test]
async fn failed_legacy_restore_retries_on_every_open_and_can_be_repaired() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("retry-legacy-restore.sqlite");
    let account_id = Uuid::new_v4();
    create_original_desktop_profile(&path, &account_id.to_string(), 70_000).await;

    for attempt in 1..=2 {
        let error = match Store::open(&path).await {
            Ok(_) => panic!("legacy restore attempt {attempt} must reject an invalid IMAP port"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("legacy account has an invalid IMAP port"),
            "{error:#}"
        );

        let inspection = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
                .expect("parse prepared profile path"),
        )
        .await
        .expect("inspect failed legacy restore");
        let legacy_account: (String, i64) = sqlx::query_as("SELECT id, port FROM legacy_accounts")
            .fetch_one(&inspection)
            .await
            .expect("read preserved legacy account");
        assert_eq!(legacy_account, (account_id.to_string(), 70_000));
        let legacy_message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM legacy_messages")
            .fetch_one(&inspection)
            .await
            .expect("count preserved legacy messages");
        assert_eq!(legacy_message_count, 1);
        let restored_account_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&inspection)
            .await
            .expect("count current accounts");
        assert_eq!(restored_account_count, 0);
        let marker_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM app_meta WHERE key IN ('legacy_desktop_profile_migrated', 'profile_schema_version')",
        )
        .fetch_one(&inspection)
        .await
        .expect("inspect completion markers");
        assert_eq!(marker_count, 0, "failed restore looked complete");
        inspection.close().await;
    }

    let repair = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse prepared profile path"),
    )
    .await
    .expect("open prepared profile for repair");
    sqlx::query("UPDATE legacy_accounts SET port = 993")
        .execute(&repair)
        .await
        .expect("repair fictional legacy port");
    repair.close().await;

    let recovered = Store::open(&path)
        .await
        .expect("repaired legacy profile completes migration");
    assert_eq!(
        recovered
            .account(account_id)
            .await
            .expect("read recovered account")
            .expect("recovered account exists")
            .email,
        "migration@example.test"
    );
    assert_eq!(
        recovered
            .message("legacy-message")
            .await
            .expect("read recovered message")
            .expect("recovered message exists")
            .subject,
        "Recoverable legacy message"
    );
}

#[tokio::test]
async fn invalid_legacy_account_id_is_preserved_and_rejected_on_every_open() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("invalid-legacy-id.sqlite");
    create_original_desktop_profile(&path, "not-a-uuid", 993).await;

    for attempt in 1..=2 {
        let error = match Store::open(&path).await {
            Ok(_) => panic!("legacy restore attempt {attempt} must reject an invalid account ID"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("has an invalid ID"),
            "{error:#}"
        );
        let inspection = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
                .expect("parse prepared profile path"),
        )
        .await
        .expect("inspect failed legacy restore");
        let preserved_id: String = sqlx::query_scalar("SELECT id FROM legacy_accounts")
            .fetch_one(&inspection)
            .await
            .expect("read preserved invalid account ID");
        assert_eq!(preserved_id, "not-a-uuid");
        let restored_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&inspection)
            .await
            .expect("count current accounts");
        assert_eq!(restored_rows, 0);
        inspection.close().await;
    }
}

#[tokio::test]
async fn legacy_drafts_and_queued_operations_are_refused_without_renaming_or_deleting_them() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("unsupported-legacy-state.sqlite");
    let account_id = Uuid::new_v4();
    create_original_desktop_profile(&path, &account_id.to_string(), 993).await;
    let fixture = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
            .expect("parse legacy fixture path"),
    )
    .await
    .expect("open legacy fixture");
    for statement in [
        "CREATE TABLE message_bodies (messageId TEXT, body TEXT)",
        "CREATE TABLE local_drafts (id TEXT, subject TEXT)",
        "CREATE TABLE operation_queue (id TEXT, operation TEXT)",
        "INSERT INTO message_bodies VALUES ('legacy-message', 'fictional cached body')",
        "INSERT INTO local_drafts VALUES ('draft-1', 'Fictional unsent draft')",
        "INSERT INTO operation_queue VALUES ('operation-1', 'mark unread')",
    ] {
        sqlx::query(statement)
            .execute(&fixture)
            .await
            .unwrap_or_else(|error| {
                panic!("legacy fixture statement failed: {statement}: {error}")
            });
    }
    fixture.close().await;

    for attempt in 1..=2 {
        let error = match Store::open(&path).await {
            Ok(_) => panic!("unsupported legacy state attempt {attempt} must be refused"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("cannot safely migrate; original data was preserved"),
            "{error:#}"
        );
        let inspection = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
                .expect("parse refused profile path"),
        )
        .await
        .expect("inspect refused legacy profile");
        let draft: String = sqlx::query_scalar("SELECT subject FROM local_drafts")
            .fetch_one(&inspection)
            .await
            .expect("read preserved draft");
        assert_eq!(draft, "Fictional unsent draft");
        let operation: String = sqlx::query_scalar("SELECT operation FROM operation_queue")
            .fetch_one(&inspection)
            .await
            .expect("read preserved queued operation");
        assert_eq!(operation, "mark unread");
        let body: String = sqlx::query_scalar("SELECT body FROM message_bodies")
            .fetch_one(&inspection)
            .await
            .expect("read preserved cached body");
        assert_eq!(body, "fictional cached body");
        for renamed in [
            "legacy_accounts",
            "legacy_local_drafts",
            "legacy_operation_queue",
        ] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
            )
            .bind(renamed)
            .fetch_one(&inspection)
            .await
            .expect("inspect renamed table");
            assert!(!exists, "{renamed} must not be created on refusal");
        }
        inspection.close().await;
    }
}

#[tokio::test]
async fn send_attempt_schema_with_matching_columns_but_missing_constraints_is_refused() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("unsafe-send-attempt-schema.sqlite");
    let store = Store::open(&path).await.expect("open profile");
    sqlx::query("DROP TABLE send_attempts")
        .execute(&store.pool)
        .await
        .expect("remove valid send journal");
    sqlx::query("CREATE TABLE send_attempts (account_id TEXT NOT NULL, attempt_id TEXT NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)")
        .execute(&store.pool)
        .await
        .expect("inject unsafe same-column schema");
    sqlx::query("DELETE FROM app_meta WHERE key = 'profile_schema_version'")
        .execute(&store.pool)
        .await
        .expect("simulate incomplete migration");
    store.pool.close().await;

    let error = match Store::open(&path).await {
        Ok(_) => panic!("schema without send-attempt constraints must be refused"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("send_attempts must have PRIMARY KEY(account_id, attempt_id)"),
        "{error:#}"
    );
}
