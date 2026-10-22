#![cfg(feature = "sqlite-fault-injection")]

use chrono::Utc;
use dakia_core::storage::{SendAttemptClaim, SendAttemptStatus, SendRecoveryStatus};
use dakia_core::{
    mail::ComposeAttachment, provider, Account, AccountDraft, ComposeMessage, MailSummary, Store,
};
use libsqlite3_sys::{SQLITE_IOERR_FSYNC, SQLITE_IOERR_WRITE};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::{
    collections::HashSet,
    fs,
    os::raw::c_int,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;
use uuid::Uuid;

const CHILD_MODE: &str = "DAKIA_SQLITE_FAULT_CHILD_MODE";
const CHILD_DB: &str = "DAKIA_SQLITE_FAULT_CHILD_DB";
const CHILD_READY: &str = "DAKIA_SQLITE_FAULT_CHILD_READY";
const ACCOUNT_ID: &str = "11111111-1111-4111-8111-111111111111";
const ATTEMPT_ID: &str = "22222222-2222-4222-8222-222222222222";
const WAL_ATTEMPT_ID: &str = "33333333-3333-4333-8333-333333333333";
const IO_FAULT_ATTEMPT_ID: &str = "44444444-4444-4444-8444-444444444444";
const RECOVERY_PREPARE_ATTEMPT_ID: &str = "55555555-5555-4555-8555-555555555555";
const RECOVERY_FINISH_ATTEMPT_ID: &str = "66666666-6666-4666-8666-666666666666";
const RECOVERY_RELEASE_ATTEMPT_ID: &str = "77777777-7777-4777-8777-777777777777";
const TEST_NAME: &str = "sqlite_write_fsync_and_torn_wal_preserve_committed_state";

extern "C" {
    static mut sqlite3_io_error_hit: c_int;
    static mut sqlite3_io_error_hardhit: c_int;
    static mut sqlite3_io_error_pending: c_int;
    static mut sqlite3_io_error_persist: c_int;
}

fn account_id() -> Uuid {
    Uuid::parse_str(ACCOUNT_ID).expect("fixed account ID")
}

fn account() -> Account {
    let mut account = AccountDraft {
        email: "sqlite-fault@example.test".into(),
        display_name: "SQLite fault fixture".into(),
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
    account.id = account_id();
    account
}

fn message(uid: i64, subject: &str) -> MailSummary {
    MailSummary {
        id: format!("{}:INBOX:{uid}", account_id()),
        account_id: ACCOUNT_ID.into(),
        mailbox: "INBOX".into(),
        uid,
        mailbox_uid_validity: Some(7),
        message_id: Some(format!("<{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("thread-{uid}"),
        subject: subject.into(),
        from_name: Some("Fictional sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "sqlite-fault@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: subject.into(),
        body_text: subject.into(),
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

fn recovery_message(subject: &str) -> ComposeMessage {
    ComposeMessage {
        account_id: account_id(),
        to: vec!["recovery-recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.into(),
        body_text: "Exact fictional recovery payload".into(),
        body_html: Some("<p>Exact fictional recovery payload</p>".into()),
        in_reply_to: None,
        references: None,
        attachments: vec![ComposeAttachment {
            filename: "recovery.txt".into(),
            mime_type: "text/plain".into(),
            content_base64: "ZmljdGlvbmFsIHJlY292ZXJ5".into(),
        }],
    }
}

#[derive(Clone, Copy, Debug)]
enum RecoveryFaultOperation {
    Prepare,
    Finish,
    Release,
}

impl RecoveryFaultOperation {
    const ALL: [Self; 3] = [Self::Prepare, Self::Finish, Self::Release];

    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Finish => "finish",
            Self::Release => "release",
        }
    }

    fn attempt_id(self) -> Uuid {
        Uuid::parse_str(match self {
            Self::Prepare => RECOVERY_PREPARE_ATTEMPT_ID,
            Self::Finish => RECOVERY_FINISH_ATTEMPT_ID,
            Self::Release => RECOVERY_RELEASE_ATTEMPT_ID,
        })
        .expect("fixed recovery fault attempt ID")
    }
}

fn wal_path(database: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", database.display()))
}

fn sqlite_extended_code(error: &anyhow::Error) -> Option<c_int> {
    let message = format!("{error:#}");
    message
        .split("(code: ")
        .nth(1)?
        .split(')')
        .next()?
        .parse()
        .ok()
}

unsafe fn arm_test_io_error(pending: c_int) {
    sqlite3_io_error_hit = 0;
    sqlite3_io_error_hardhit = 0;
    sqlite3_io_error_persist = 0;
    sqlite3_io_error_pending = pending;
}

unsafe fn disarm_test_io_error() {
    sqlite3_io_error_pending = 0;
    sqlite3_io_error_persist = 0;
}

fn copy_profile(source: &Path, destination: &Path) {
    fs::copy(source, destination).expect("copy profile database");
    fs::copy(
        source
            .parent()
            .expect("profile directory")
            .join("vault.key"),
        destination
            .parent()
            .expect("fault-case directory")
            .join("vault.key"),
    )
    .expect("copy fictional vault key");
}

fn run_child(mode: &str, database: &Path, ready: Option<&Path>) {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_MODE, mode)
        .env(CHILD_DB, database);
    if let Some(ready) = ready {
        command.env(CHILD_READY, ready);
    }
    let output = command.output().expect("launch SQLite fault child");
    assert!(
        output.status.success(),
        "SQLite fault child {mode} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn terminate(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().expect("terminate stable WAL writer");
            child.wait().expect("reap WAL writer");
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_wal_writer(database: &Path, ready: &Path) -> ChildGuard {
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_MODE, "wal-writer")
        .env(CHILD_DB, database)
        .env(CHILD_READY, ready)
        .spawn()
        .expect("launch WAL writer child");
    ChildGuard(Some(child))
}

async fn child_case(mode: &str, database: &Path) {
    match mode {
        "seed" => {
            let store = Store::open(database).await.expect("open seed profile");
            store.save_account(&account()).await.expect("seed account");
            store
                .upsert_messages(&[message(1, "Committed baseline")])
                .await
                .expect("seed committed message");
            store
                .set_secret("sqlite-fault-secret", "preserve credential")
                .await
                .expect("seed encrypted credential");
            assert!(matches!(
                store
                    .claim_send_attempt(
                        account_id(),
                        Uuid::parse_str(ATTEMPT_ID).expect("fixed attempt ID"),
                        "committed-payload",
                    )
                    .await
                    .expect("seed pending action"),
                SendAttemptClaim::Claimed
            ));
            drop(store);
            checkpoint_seed_profile(database).await;
        }
        "exact-io-faults" => {
            let cases = tempdir().expect("I/O fault cases");
            let mut observed = HashSet::new();
            for pending in 1..=128 {
                let case_directory = cases.path().join(format!("pending-{pending}"));
                fs::create_dir(&case_directory).expect("create fault case directory");
                let case_database = case_directory.join("profile.sqlite");
                copy_profile(database, &case_database);
                let store = Store::open(&case_database)
                    .await
                    .expect("open copied fault profile");
                unsafe { arm_test_io_error(pending) };
                let result = store
                    .claim_send_attempt(
                        account_id(),
                        Uuid::parse_str(IO_FAULT_ATTEMPT_ID).expect("fixed I/O fault attempt ID"),
                        "io-fault-candidate-payload",
                    )
                    .await;
                unsafe { disarm_test_io_error() };
                let Some(code) = result
                    .as_ref()
                    .err()
                    .and_then(|error| sqlite_extended_code(error))
                else {
                    continue;
                };
                if code != SQLITE_IOERR_WRITE && code != SQLITE_IOERR_FSYNC {
                    continue;
                }
                observed.insert(code);
                drop(store);
                verify_io_fault_profile(&case_database).await;
                if observed.contains(&SQLITE_IOERR_WRITE) && observed.contains(&SQLITE_IOERR_FSYNC)
                {
                    break;
                }
            }
            assert!(
                observed.contains(&SQLITE_IOERR_WRITE),
                "bounded fault scan did not reach SQLite xWrite error code {SQLITE_IOERR_WRITE}"
            );
            assert!(
                observed.contains(&SQLITE_IOERR_FSYNC),
                "bounded fault scan did not reach SQLite xSync error code {SQLITE_IOERR_FSYNC}"
            );
        }
        "recovery-io-faults" => {
            run_recovery_io_fault_scans(database).await;
        }
        "wal-writer" => {
            let store = Store::open(database).await.expect("open WAL profile");
            let claim = store
                .claim_send_attempt(
                    account_id(),
                    Uuid::parse_str(WAL_ATTEMPT_ID).expect("fixed WAL attempt ID"),
                    "wal-candidate-payload",
                )
                .await
                .expect("commit candidate into live WAL");
            assert!(matches!(claim, SendAttemptClaim::Claimed));
            let ready = std::env::var_os(CHILD_READY).expect("writer ready path");
            fs::write(ready, b"ready").expect("publish WAL-ready barrier");
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        "verify-complete-wal" => {
            verify_recovered_profile(database, Some(true)).await;
        }
        "verify-torn-wal" => {
            verify_recovered_profile(database, Some(false)).await;
        }
        other => panic!("unknown SQLite fault child mode: {other}"),
    }
}

async fn checkpoint_seed_profile(database: &Path) {
    let options = SqliteConnectOptions::new()
        .filename(database)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open seed profile for checkpoint");
    let (busy, remaining, checkpointed): (i64, i64, i64) =
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&pool)
            .await
            .expect("checkpoint seed WAL");
    assert_eq!(busy, 0, "seed WAL checkpoint was busy");
    assert_eq!(
        remaining, checkpointed,
        "seed WAL checkpoint left unapplied frames"
    );
    pool.close().await;
}

async fn verify_recovered_profile(database: &Path, wal_attempt_present: Option<bool>) {
    let store = Store::open(database)
        .await
        .expect("reopen recovered profile");
    verify_committed_baseline(&store).await;
    if let Some(expected_present) = wal_attempt_present {
        let claim = store
            .claim_send_attempt(
                account_id(),
                Uuid::parse_str(WAL_ATTEMPT_ID).expect("fixed WAL attempt ID"),
                "wal-candidate-payload",
            )
            .await
            .expect("inspect WAL candidate attempt");
        if expected_present {
            assert!(matches!(
                claim,
                SendAttemptClaim::Existing(SendAttemptStatus::Pending)
            ));
        } else {
            assert!(matches!(claim, SendAttemptClaim::Claimed));
        }
    }
}

async fn verify_io_fault_profile(database: &Path) {
    let store = Store::open(database)
        .await
        .expect("reopen profile after SQLite I/O error");
    verify_committed_baseline(&store).await;

    // SQLITE_TEST reports WRITE and FSYNC errors after the underlying default
    // VFS call. The failed API call is therefore allowed to recover as either
    // fully committed or fully absent. A matching Existing result proves the
    // complete journal row survived; Claimed proves the prior row was absent
    // and could be safely created on retry. Any partial or mismatched row fails.
    let claim = store
        .claim_send_attempt(
            account_id(),
            Uuid::parse_str(IO_FAULT_ATTEMPT_ID).expect("fixed I/O fault attempt ID"),
            "io-fault-candidate-payload",
        )
        .await
        .expect("inspect I/O fault candidate attempt");
    assert!(matches!(
        claim,
        SendAttemptClaim::Claimed | SendAttemptClaim::Existing(SendAttemptStatus::Pending)
    ));
}

async fn prepare_recovery_fault_operation(store: &Store, operation: RecoveryFaultOperation) {
    if matches!(operation, RecoveryFaultOperation::Prepare) {
        return;
    }
    let attempt_id = operation.attempt_id();
    let message = recovery_message(&format!("{} fault fixture", operation.name()));
    assert!(matches!(
        store
            .prepare_send_recovery(account_id(), attempt_id, &message)
            .await
            .expect("prepare recovery transition fault fixture"),
        dakia_core::SendRecoveryClaim::Claimed
    ));
}

async fn apply_recovery_fault_operation(
    store: &Store,
    operation: RecoveryFaultOperation,
) -> anyhow::Result<()> {
    let attempt_id = operation.attempt_id();
    match operation {
        RecoveryFaultOperation::Prepare => {
            store
                .prepare_send_recovery(
                    account_id(),
                    attempt_id,
                    &recovery_message("prepare fault fixture"),
                )
                .await?;
        }
        RecoveryFaultOperation::Finish => {
            store
                .finish_send_recovery(account_id(), attempt_id, SendAttemptStatus::Submitted)
                .await?;
        }
        RecoveryFaultOperation::Release => {
            store
                .release_send_recovery(account_id(), attempt_id)
                .await?;
        }
    }
    Ok(())
}

async fn run_recovery_io_fault_scans(database: &Path) {
    let cases = tempdir().expect("recovery I/O fault cases");
    for operation in RecoveryFaultOperation::ALL {
        let mut observed = HashSet::new();
        for pending in 1..=128 {
            let case_directory = cases
                .path()
                .join(format!("{}-pending-{pending}", operation.name()));
            fs::create_dir(&case_directory).expect("create recovery fault case directory");
            let case_database = case_directory.join("profile.sqlite");
            copy_profile(database, &case_database);
            let store = Store::open(&case_database)
                .await
                .expect("open copied recovery fault profile");
            prepare_recovery_fault_operation(&store, operation).await;
            unsafe { arm_test_io_error(pending) };
            let result = apply_recovery_fault_operation(&store, operation).await;
            unsafe { disarm_test_io_error() };
            let Some(code) = result
                .as_ref()
                .err()
                .and_then(|error| sqlite_extended_code(error))
            else {
                continue;
            };
            if code != SQLITE_IOERR_WRITE && code != SQLITE_IOERR_FSYNC {
                continue;
            }
            observed.insert(code);
            drop(store);
            verify_recovery_fault_profile(&case_database, operation).await;
            if observed.contains(&SQLITE_IOERR_WRITE) && observed.contains(&SQLITE_IOERR_FSYNC) {
                break;
            }
        }
        assert!(
            observed.contains(&SQLITE_IOERR_WRITE),
            "{} fault scan did not reach SQLite xWrite error code {SQLITE_IOERR_WRITE}",
            operation.name()
        );
        assert!(
            observed.contains(&SQLITE_IOERR_FSYNC),
            "{} fault scan did not reach SQLite xSync error code {SQLITE_IOERR_FSYNC}",
            operation.name()
        );
    }
}

async fn verify_recovery_fault_profile(database: &Path, operation: RecoveryFaultOperation) {
    let store = Store::open(database)
        .await
        .expect("reopen recovery profile after SQLite I/O error");
    verify_committed_baseline(&store).await;
    let attempt_id = operation.attempt_id();
    let recovery = store
        .load_send_recovery(account_id(), attempt_id)
        .await
        .expect("read recovery fault state after reopen");
    let status = recovery.as_ref().map(|recovery| recovery.status);
    drop(store);

    let options = SqliteConnectOptions::new()
        .filename(database)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("inspect recovery transition rows");
    let rows: Vec<(String, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT r.payload_hash, r.retryable, j.status, j.payload_hash FROM send_recovery_payloads r LEFT JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id = ? AND r.attempt_id = ?",
    )
    .bind(account_id().to_string())
    .bind(attempt_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("read recovery/journal pair");
    let journal: Option<(String, String)> = sqlx::query_as(
        "SELECT status, payload_hash FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(account_id().to_string())
    .bind(attempt_id.to_string())
    .fetch_optional(&pool)
    .await
    .expect("read standalone journal state");
    pool.close().await;

    match operation {
        RecoveryFaultOperation::Prepare => match (status, rows.as_slice(), journal) {
            (None, [], None) => {}
            (
                Some(SendRecoveryStatus::Pending),
                [(recovery_hash, 0, Some(journal_status), Some(journal_hash))],
                Some((standalone_status, standalone_hash)),
            ) if recovery_hash == journal_hash
                && journal_status == "pending"
                && standalone_status == "pending"
                && standalone_hash == *recovery_hash => {}
            state => panic!("prepare fault left a partial recovery/journal state: {state:?}"),
        },
        RecoveryFaultOperation::Finish => match (status, rows.as_slice(), journal) {
            (
                Some(SendRecoveryStatus::Pending),
                [(recovery_hash, 0, Some(journal_status), Some(journal_hash))],
                Some((standalone_status, standalone_hash)),
            ) if recovery_hash == journal_hash
                && journal_status == "pending"
                && standalone_status == "pending"
                && standalone_hash == *recovery_hash => {}
            (None, [], Some((journal_status, _))) if journal_status == "submitted" => {}
            state => panic!("finish fault left a partial recovery/journal state: {state:?}"),
        },
        RecoveryFaultOperation::Release => match (status, rows.as_slice(), journal) {
            (
                Some(SendRecoveryStatus::Pending),
                [(recovery_hash, 0, Some(journal_status), Some(journal_hash))],
                Some((standalone_status, standalone_hash)),
            ) if recovery_hash == journal_hash
                && journal_status == "pending"
                && standalone_status == "pending"
                && standalone_hash == *recovery_hash => {}
            (Some(SendRecoveryStatus::NotSubmitted), [(recovery_hash, 1, None, None)], None)
                if !recovery_hash.is_empty() => {}
            state => panic!("release fault left a partial recovery/journal state: {state:?}"),
        },
    }
}

async fn verify_committed_baseline(store: &Store) {
    assert!(store
        .message(&message(1, "Committed baseline").id)
        .await
        .expect("read baseline")
        .is_some());
    assert_eq!(
        store
            .secret("sqlite-fault-secret")
            .await
            .expect("read preserved credential")
            .as_deref(),
        Some("preserve credential")
    );
    assert!(matches!(
        store
            .claim_send_attempt(
                account_id(),
                Uuid::parse_str(ATTEMPT_ID).expect("fixed attempt ID"),
                "committed-payload",
            )
            .await
            .expect("read pending action"),
        SendAttemptClaim::Existing(SendAttemptStatus::Pending)
    ));
}

#[test]
fn sqlite_write_fsync_and_torn_wal_preserve_committed_state() {
    if let Ok(mode) = std::env::var(CHILD_MODE) {
        let database = PathBuf::from(std::env::var_os(CHILD_DB).expect("child database path"));
        tokio::runtime::Runtime::new()
            .expect("child Tokio runtime")
            .block_on(child_case(&mode, &database));
        return;
    }

    let directory = tempdir().expect("fault harness directory");
    let database = directory.path().join("profile.sqlite");
    run_child("seed", &database, None);
    let seed_wal = wal_path(&database);
    assert!(
        !seed_wal.exists() || fs::metadata(seed_wal).expect("inspect seed WAL").len() == 0,
        "seed child did not checkpoint its baseline before fault copies"
    );

    // The standalone SQLite build enables SQLITE_TEST. Its process-global
    // countdown injects failures inside the actual unix xWrite and xSync
    // methods. Require the exact extended error codes rather than treating a
    // generic commit failure as write or fsync evidence.
    run_child("exact-io-faults", &database, None);
    run_child("recovery-io-faults", &database, None);

    let ready = directory.path().join("wal-ready");
    let mut writer = spawn_wal_writer(&database, &ready);
    let deadline = Instant::now() + Duration::from_secs(10);
    while (!ready.exists() || !wal_path(&database).exists()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(ready.exists(), "WAL writer did not reach the copy barrier");
    let source_wal = wal_path(&database);
    assert!(source_wal.exists(), "WAL writer did not leave a live WAL");

    let complete_directory = tempdir().expect("complete WAL control directory");
    let complete_database = complete_directory.path().join("profile.sqlite");
    fs::copy(&database, &complete_database).expect("copy complete control database");
    fs::copy(&source_wal, wal_path(&complete_database)).expect("copy complete control WAL");
    fs::copy(
        directory.path().join("vault.key"),
        complete_directory.path().join("vault.key"),
    )
    .expect("copy complete-control vault key");

    let torn_directory = tempdir().expect("torn WAL directory");
    let torn_database = torn_directory.path().join("profile.sqlite");
    fs::copy(&database, &torn_database).expect("copy stable main database");
    fs::copy(&source_wal, wal_path(&torn_database)).expect("copy live WAL");
    fs::copy(
        directory.path().join("vault.key"),
        torn_directory.path().join("vault.key"),
    )
    .expect("copy fictional vault key");

    writer.terminate();

    // The unmodified copy proves that the final complete transaction contains
    // the WAL candidate before the identical second copy is truncated.
    run_child("verify-complete-wal", &complete_database, None);

    let torn_wal = wal_path(&torn_database);
    let wal_length = fs::metadata(&torn_wal).expect("inspect copied WAL").len();
    assert!(wal_length > 45, "copied WAL has no truncatable frame");
    fs::OpenOptions::new()
        .write(true)
        .open(&torn_wal)
        .expect("open copied WAL")
        .set_len(wal_length - 13)
        .expect("truncate the final WAL frame");

    run_child("verify-torn-wal", &torn_database, None);
}
