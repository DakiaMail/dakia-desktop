use chrono::Utc;
use dakia_core::{
    provider, Account, AccountDraft, AccountId, CachedMessageContent, MailRebuildJob, MailSummary,
    Store,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Barrier,
    task::JoinSet,
    time::{sleep, timeout},
};
use uuid::Uuid;

const PROCESS_DEADLINE: Duration = Duration::from_secs(10);
const CANCELLATION_DEADLINE: Duration = Duration::from_secs(2);
const CLAIM_WORKER_READY: &str = "DAKIA_CROSS_PROCESS_CLAIM_READY";
const TRANSACTION_WORKER_READY: &str = "DAKIA_TRANSACTION_WORKER_READY";
const CHECKPOINT_WORKER_READY: &str = "DAKIA_CHECKPOINT_WORKER_READY";

async fn run(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dakia"));
    command.args(args);
    run_command(command).await
}

async fn run_with_data_dir(args: &[&str], data_dir: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dakia"));
    command.args(args).env("DAKIA_DATA_DIR", data_dir);
    run_command(command).await
}

async fn run_command(mut command: Command) -> Output {
    command.kill_on_drop(true);
    timeout(PROCESS_DEADLINE, command.output())
        .await
        .expect("CLI subprocess exceeded its deadline")
        .expect("CLI subprocess could not be started")
}

/// Starts this integration-test binary in a separate process, not another
/// task in this test runtime. The storage owner token is process-scoped, so
/// this is the boundary that catches a fresh claim being stolen on open.
async fn spawn_claim_worker(database: &Path, message_id: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args([
            "--exact",
            "cross_process_content_claim_worker",
            "--nocapture",
        ])
        .env("DAKIA_CLI_CLAIM_WORKER_DATABASE", database)
        .env("DAKIA_CLI_CLAIM_WORKER_MESSAGE_ID", message_id)
        .env("DAKIA_CLI_CLAIM_WORKER_HOLD_MILLIS", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn isolated claim worker");
    let stdout = child.stdout.take().expect("worker stdout");
    let mut lines = BufReader::new(stdout).lines();

    timeout(PROCESS_DEADLINE, async {
        while let Some(line) = lines.next_line().await.expect("read worker stdout") {
            if line == CLAIM_WORKER_READY {
                return;
            }
        }
        panic!("claim worker exited before acquiring its lease");
    })
    .await
    .expect("claim worker did not acquire its lease before the deadline");
    child
}

/// Starts an isolated process immediately before or after a public Store
/// transaction. Waiting for its readiness marker makes the kill point
/// deterministic instead of relying on scheduler timing.
async fn spawn_transaction_worker(database: &Path, account_id: AccountId, phase: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args(["--exact", "cross_process_transaction_worker", "--nocapture"])
        .env("DAKIA_CLI_TRANSACTION_WORKER_DATABASE", database)
        .env(
            "DAKIA_CLI_TRANSACTION_WORKER_ACCOUNT_ID",
            account_id.to_string(),
        )
        .env("DAKIA_CLI_TRANSACTION_WORKER_PHASE", phase)
        .env("DAKIA_CLI_TRANSACTION_WORKER_HOLD_MILLIS", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn transaction worker");
    let stdout = child.stdout.take().expect("transaction worker stdout");
    let mut lines = BufReader::new(stdout).lines();

    timeout(PROCESS_DEADLINE, async {
        while let Some(line) = lines
            .next_line()
            .await
            .expect("read transaction worker stdout")
        {
            if line == TRANSACTION_WORKER_READY {
                return;
            }
        }
        panic!("transaction worker exited before its requested kill point");
    })
    .await
    .expect("transaction worker did not reach its kill point before the deadline");
    child
}

/// Stops an isolated process immediately before or after one named durable
/// checkpoint. Each operation uses a public Store method, so this observes the
/// same SQLite transaction boundary as production without reaching into the
/// storage implementation from the test.
async fn spawn_checkpoint_worker(
    database: &Path,
    account_id: AccountId,
    message_id: &str,
    operation: &str,
    phase: &str,
) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args(["--exact", "cross_process_checkpoint_worker", "--nocapture"])
        .env("DAKIA_CLI_CHECKPOINT_WORKER_DATABASE", database)
        .env(
            "DAKIA_CLI_CHECKPOINT_WORKER_ACCOUNT_ID",
            account_id.to_string(),
        )
        .env("DAKIA_CLI_CHECKPOINT_WORKER_MESSAGE_ID", message_id)
        .env("DAKIA_CLI_CHECKPOINT_WORKER_OPERATION", operation)
        .env("DAKIA_CLI_CHECKPOINT_WORKER_PHASE", phase)
        .env("DAKIA_CLI_CHECKPOINT_WORKER_HOLD_MILLIS", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn checkpoint worker");
    let stdout = child.stdout.take().expect("checkpoint worker stdout");
    let mut lines = BufReader::new(stdout).lines();

    timeout(PROCESS_DEADLINE, async {
        while let Some(line) = lines
            .next_line()
            .await
            .expect("read checkpoint worker stdout")
        {
            if line == CHECKPOINT_WORKER_READY {
                return;
            }
        }
        panic!("checkpoint worker exited before its requested kill point");
    })
    .await
    .expect("checkpoint worker did not reach its kill point before the deadline");
    child
}

async fn kill_worker(mut worker: Child) {
    worker.start_kill().expect("start worker kill");
    let status = timeout(CANCELLATION_DEADLINE, worker.wait())
        .await
        .expect("worker did not exit within the cancellation deadline")
        .expect("reap killed worker");
    assert!(
        !status.success(),
        "killed worker must not exit successfully"
    );
}

async fn seed_profile(directory: &Path, label: &str) -> (Account, MailSummary) {
    let store = Store::open(directory.join("dakia.db"))
        .await
        .expect("open seed store");
    let account = AccountDraft {
        email: format!("{label}@example.test"),
        display_name: format!("{label} account"),
        provider_id: Some("custom".into()),
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
    .into_account(provider::by_id("custom").expect("custom provider"));
    store.save_account(&account).await.expect("save account");

    let message = MailSummary {
        id: format!("{label}-message"),
        account_id: account.id.to_string(),
        mailbox: "INBOX".into(),
        uid: 1,
        message_id: Some(format!("<{label}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("{label}-thread"),
        mailbox_uid_validity: None,
        subject: format!("{label} durable subject"),
        from_name: Some("Fixture sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: account.email.clone(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: format!("{label} durable body"),
        body_text: format!("{label} durable body"),
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
    };
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("seed message");
    drop(store);
    (account, message)
}

fn assert_success_json(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stderr, b"");
    serde_json::from_slice(&output.stdout).expect("CLI stdout is JSON")
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[tokio::test]
async fn help_version_and_invalid_input_do_not_create_profile_state() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("never-created");
    let profile_arg = profile.to_string_lossy().into_owned();

    let help = run(&["--data-dir", &profile_arg, "--help"]).await;
    assert_eq!(help.status.code(), Some(0));
    assert_eq!(help.stderr, b"");
    assert_eq!(
        help.stdout,
        concat!(
            "Search, read, and send mail from the terminal\n\n",
            "Usage: dakia [OPTIONS] <COMMAND>\n\n",
            "Commands:\n",
            "  account     \n  sync        \n  classify    \n  search      \n",
            "  show        \n  attachment  \n  archive     \n  spam        \n",
            "  trash       \n  delete      \n  send        \n  ai          \n",
            "  help        Print this message or the help of the given subcommand(s)\n\n",
            "Options:\n",
            "      --data-dir <DATA_DIR>  [env: DAKIA_DATA_DIR=]\n",
            "      --json                 \n",
            "  -h, --help                 Print help\n",
            "  -V, --version              Print version\n"
        )
        .as_bytes()
    );

    let version = run(&["--data-dir", &profile_arg, "--version"]).await;
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(
        version.stdout,
        format!("dakia {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert_eq!(version.stderr, b"");

    let invalid = run(&["--data-dir", &profile_arg, "not-a-command"]).await;
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(invalid.stdout, b"");
    assert_eq!(
        invalid.stderr,
        concat!(
            "error: unrecognized subcommand 'not-a-command'\n\n",
            "Usage: dakia [OPTIONS] <COMMAND>\n\n",
            "For more information, try '--help'.\n"
        )
        .as_bytes()
    );

    assert!(!profile.join("dakia.db").exists());
    assert!(!profile.join("vault.key").exists());
}

#[tokio::test]
async fn data_dir_overrides_environment_and_account_list_has_a_stable_json_schema() {
    let temporary = TempDir::new().expect("temporary directory");
    let environment_profile = temporary.path().join("environment");
    let argument_profile = temporary.path().join("argument");
    let (environment_account, _) = seed_profile(&environment_profile, "environment").await;
    let (argument_account, _) = seed_profile(&argument_profile, "argument").await;
    let argument_profile_arg = argument_profile.to_string_lossy().into_owned();

    let output = run_with_data_dir(
        &[
            "--data-dir",
            &argument_profile_arg,
            "--json",
            "account",
            "list",
        ],
        &environment_profile,
    )
    .await;
    let accounts = assert_success_json(&output);
    let accounts = accounts.as_array().expect("account list is an array");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], argument_account.id.to_string());
    assert_eq!(accounts[0]["email"], argument_account.email);
    assert_ne!(accounts[0]["id"], environment_account.id.to_string());
    assert_eq!(
        keys(&accounts[0]),
        BTreeSet::from([
            "account_name",
            "archive_mailbox",
            "auth",
            "created_at",
            "display_name",
            "email",
            "enabled",
            "id",
            "imap_host",
            "imap_port",
            "imap_security",
            "provider_id",
            "smtp_host",
            "smtp_port",
            "smtp_security",
            "spam_mailbox",
        ])
    );
    assert_eq!(
        keys(&accounts[0]["auth"]),
        BTreeSet::from(["type", "username"])
    );
    assert_eq!(accounts[0]["auth"]["type"], "password");
}

#[tokio::test]
async fn sync_rejects_an_unknown_account_and_preserves_the_ndjson_contract() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("profile");
    let profile_arg = profile.to_string_lossy().into_owned();
    let unknown = Uuid::from_u128(0xfeed);

    let missing = run(&[
        "--data-dir",
        &profile_arg,
        "--json",
        "sync",
        "--account",
        &unknown.to_string(),
    ])
    .await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(missing.stdout, b"");
    assert_eq!(missing.stderr, b"Error: account not found\n");

    let empty = run(&["--data-dir", &profile_arg, "--json", "sync"]).await;
    assert_eq!(empty.status.code(), Some(0));
    assert_eq!(empty.stderr, b"");
    assert_eq!(empty.stdout, b"");
}

#[tokio::test]
async fn restart_preserves_seeded_state_without_cross_profile_leakage() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile_a = temporary.path().join("profile-a");
    let profile_b = temporary.path().join("profile-b");
    let (account_a, message_a) = seed_profile(&profile_a, "alpha").await;
    let (account_b, message_b) = seed_profile(&profile_b, "bravo").await;
    let profile_a_arg = profile_a.to_string_lossy().into_owned();
    let profile_b_arg = profile_b.to_string_lossy().into_owned();

    for _ in 0..2 {
        let output = run(&[
            "--data-dir",
            &profile_a_arg,
            "--json",
            "search",
            "durable",
            "--account",
            &account_a.id.to_string(),
        ])
        .await;
        let messages = assert_success_json(&output);
        let messages = messages.as_array().expect("search result is an array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], message_a.id);
        assert_eq!(messages[0]["account_id"], account_a.id.to_string());
        assert_ne!(messages[0]["id"], message_b.id);
    }

    let output = run(&["--data-dir", &profile_b_arg, "--json", "search", "durable"]).await;
    let messages = assert_success_json(&output);
    let messages = messages.as_array().expect("search result is an array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], message_b.id);
    assert_eq!(messages[0]["account_id"], account_b.id.to_string());
    assert_ne!(messages[0]["id"], message_a.id);
}

#[tokio::test]
async fn trash_and_delete_refuse_without_yes_and_leave_durable_state_unchanged() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("profile");
    let (_, message) = seed_profile(&profile, "refusal").await;
    let profile_arg = profile.to_string_lossy().into_owned();

    for (command, expected_error) in [
        ("trash", "Error: refusing to trash without --yes\n"),
        (
            "delete",
            "Error: refusing to permanently delete without --yes\n",
        ),
    ] {
        let output = run(&["--data-dir", &profile_arg, command, &message.id]).await;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stdout, b"");
        assert_eq!(output.stderr, expected_error.as_bytes());

        let store = Store::open(profile.join("dakia.db"))
            .await
            .expect("reopen after refusal");
        let stored = store.message(&message.id).await.expect("read message");
        assert_eq!(
            stored.as_ref().map(|item| &item.mailbox),
            Some(&"INBOX".into())
        );
        assert_eq!(stored.as_ref().map(|item| item.uid), Some(1));
        drop(store);
    }
}

#[tokio::test]
async fn concurrent_fresh_profile_startup_is_bounded_and_restartable() {
    let temporary = TempDir::new().expect("temporary directory");
    let profiles = (0..3)
        .map(|index| temporary.path().join(format!("fresh-profile-{index}")))
        .collect::<Vec<_>>();
    let mut processes = JoinSet::new();

    // Separate OS processes exercise the real advisory migration boundary.
    // Multiple independent profiles prevent one lucky scheduling order from
    // hiding the FTS constructor race that originally failed a full run.
    for profile in &profiles {
        let profile_arg = profile.to_string_lossy().into_owned();
        for _ in 0..8 {
            let profile_arg = profile_arg.clone();
            processes.spawn(async move {
                run(&["--data-dir", &profile_arg, "--json", "account", "list"]).await
            });
        }
    }
    while let Some(joined) = processes.join_next().await {
        let output = joined.expect("startup task did not panic");
        assert_eq!(
            output.status.code(),
            Some(0),
            "unexpected concurrent startup output: stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(output.stdout, b"[]\n");
        assert_eq!(output.stderr, b"");
    }

    for profile in profiles {
        assert_eq!(
            std::fs::read(profile.join("vault.key"))
                .expect("vault key")
                .len(),
            32
        );
        assert!(profile.join("dakia.db").is_file());
        let store = Store::open(profile.join("dakia.db"))
            .await
            .expect("reopen concurrently initialized profile");
        assert!(store
            .accounts()
            .await
            .expect("read initialized store")
            .is_empty());
        drop(store);

        let profile_arg = profile.to_string_lossy().into_owned();
        let restart = run(&["--data-dir", &profile_arg, "--json", "account", "list"]).await;
        assert_eq!(restart.status.code(), Some(0));
        assert_eq!(restart.stdout, b"[]\n");
        assert_eq!(restart.stderr, b"");
    }
}

#[tokio::test]
async fn open_store_and_real_cli_share_a_profile_during_concurrent_reads_and_writes() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("shared-profile");
    let (account, message) = seed_profile(&profile, "shared-profile").await;
    let profile_arg = profile.to_string_lossy().into_owned();
    let desktop_side = Store::open(profile.join("dakia.db"))
        .await
        .expect("keep desktop-side Store open");
    let participant_count = 9;
    let barrier = std::sync::Arc::new(Barrier::new(participant_count));
    let mut processes = JoinSet::new();

    for _ in 0..8 {
        let barrier = barrier.clone();
        let profile_arg = profile_arg.clone();
        let account_id = account.id.to_string();
        processes.spawn(async move {
            barrier.wait().await;
            run(&[
                "--data-dir",
                &profile_arg,
                "--json",
                "search",
                "durable",
                "--account",
                &account_id,
            ])
            .await
        });
    }

    barrier.wait().await;
    for iteration in 0..40 {
        desktop_side
            .set_message_read(&message.id, iteration % 2 == 0)
            .await
            .expect("desktop-side write succeeds while CLI readers are active");
        tokio::task::yield_now().await;
    }
    desktop_side
        .set_message_read(&message.id, true)
        .await
        .expect("commit deterministic final desktop-side state");

    while let Some(joined) = processes.join_next().await {
        let output = joined.expect("CLI search task did not panic");
        let messages = assert_success_json(&output);
        let messages = messages.as_array().expect("CLI search returns an array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], message.id);
        assert_eq!(messages[0]["account_id"], account.id.to_string());
    }

    let from_open_store = desktop_side
        .message(&message.id)
        .await
        .expect("desktop-side Store remains readable")
        .expect("shared message remains present");
    assert!(from_open_store.is_read);
    assert_eq!(from_open_store.subject, message.subject);

    let final_cli = run(&[
        "--data-dir",
        &profile_arg,
        "--json",
        "search",
        "durable",
        "--account",
        &account.id.to_string(),
    ])
    .await;
    let messages = assert_success_json(&final_cli);
    let messages = messages
        .as_array()
        .expect("final CLI search returns an array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], message.id);
    assert_eq!(messages[0]["is_read"], true);
}

#[tokio::test]
async fn cancelling_a_blocking_worker_exits_bounded_without_corrupting_profile_state() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("profile");
    let (account, message) = seed_profile(&profile, "cross-process-lease").await;
    let database = profile.join("dakia.db");
    let profile_arg = profile.to_string_lossy().into_owned();

    let mut worker = spawn_claim_worker(&database, &message.id).await;
    let store = Store::open(&database)
        .await
        .expect("open profile while another process fetches content");

    assert!(
        !store
            .claim_message_content_fetch(&message.id)
            .await
            .expect("respect live worker lease"),
        "a second process must not steal a fresh content-fetch lease"
    );

    worker
        .start_kill()
        .expect("start cancellation of blocking worker");
    let status = timeout(CANCELLATION_DEADLINE, worker.wait())
        .await
        .expect("blocking worker did not exit within the cancellation deadline")
        .expect("reap cancelled worker");
    assert!(
        !status.success(),
        "cancelled worker must not report a clean completion"
    );

    assert!(
        !store
            .claim_message_content_fetch(&message.id)
            .await
            .expect("preserve fresh lease after owner crash"),
        "cancellation must retain the fresh lease until its bounded stale timeout"
    );
    drop(store);

    // The abruptly stopped worker has made its one transient lease durable,
    // but it must not leave a partial account or message write behind. Reopen
    // first through Store, then through the CLI's normal startup path.
    let reopened = Store::open(&database)
        .await
        .expect("reopen profile after worker cancellation");
    let accounts = reopened.accounts().await.expect("read restarted profile");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, account.id);
    let stored = reopened
        .message(&message.id)
        .await
        .expect("read durable message after cancellation")
        .expect("seed message survives cancellation");
    assert_eq!(stored.account_id, account.id.to_string());
    assert_eq!(stored.mailbox, "INBOX");
    assert_eq!(stored.uid, 1);
    assert_eq!(stored.subject, message.subject);
    assert_eq!(stored.snippet, message.snippet);
    assert_eq!(stored.content_state, "complete");
    drop(reopened);

    let restart = run(&[
        "--data-dir",
        &profile_arg,
        "--json",
        "search",
        "durable",
        "--account",
        &account.id.to_string(),
    ])
    .await;
    let messages = assert_success_json(&restart);
    let messages = messages
        .as_array()
        .expect("restart search result is an array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], message.id);
    assert_eq!(messages[0]["subject"], message.subject);
}

#[tokio::test]
async fn process_kills_before_and_after_a_store_transaction_preserve_only_committed_state() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("profile");
    let (account, original) = seed_profile(&profile, "transaction-owner").await;
    let (other_account, other_message) = seed_profile(&profile, "transaction-neighbor").await;
    let database = profile.join("dakia.db");

    // The first worker has opened the real profile but has not entered the
    // move transaction. Killing it must leave the original locator intact.
    kill_worker(spawn_transaction_worker(&database, account.id, "before").await).await;
    let store = Store::open(&database)
        .await
        .expect("reopen after pre-transaction kill");
    assert!(store
        .message_by_locator(account.id, "INBOX", 1)
        .await
        .expect("read source after pre-transaction kill")
        .is_some());
    assert!(store
        .message_by_locator(account.id, "Archive", 99)
        .await
        .expect("read destination after pre-transaction kill")
        .is_none());
    assert!(store
        .message_by_locator(other_account.id, "INBOX", 1)
        .await
        .expect("read neighboring account")
        .is_some());
    drop(store);

    // The second worker reports readiness only after move_message has
    // committed. Killing the process after that marker must not roll the
    // transaction back or expose both source and destination.
    kill_worker(spawn_transaction_worker(&database, account.id, "after").await).await;
    for _ in 0..3 {
        let store = Store::open(&database)
            .await
            .expect("reopen after post-transaction kill");
        assert!(store
            .message_by_locator(account.id, "INBOX", 1)
            .await
            .expect("read source after committed move")
            .is_none());
        let moved = store
            .message_by_locator(account.id, "Archive", 99)
            .await
            .expect("read committed destination")
            .expect("committed destination survives restart");
        assert_eq!(moved.subject, original.subject);
        let neighbor = store
            .message_by_locator(other_account.id, "INBOX", 1)
            .await
            .expect("read neighboring account after kill")
            .expect("neighboring account survives kill");
        assert_eq!(neighbor.id, other_message.id);
        drop(store);
    }

    // Simulate a stale provider fetch that started before the committed move.
    // Its old source row must remain suppressed by the durable tombstone.
    let store = Store::open(&database)
        .await
        .expect("reopen for stale publication");
    store
        .upsert_messages(std::slice::from_ref(&original))
        .await
        .expect("publish stale source response");
    assert!(store
        .message_by_locator(account.id, "INBOX", 1)
        .await
        .expect("check stale source suppression")
        .is_none());
    assert!(store
        .message_by_locator(account.id, "Archive", 99)
        .await
        .expect("check committed destination")
        .is_some());
}

#[tokio::test]
async fn process_kills_around_rebuild_and_content_cache_commits_reopen_only_complete_state() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("profile");
    let (account, message) = seed_profile(&profile, "checkpoint-owner").await;
    let database = profile.join("dakia.db");

    for operation in ["rebuild", "content-cache"] {
        kill_worker(
            spawn_checkpoint_worker(&database, account.id, &message.id, operation, "before").await,
        )
        .await;

        let store = Store::open(&database)
            .await
            .expect("reopen after pre-checkpoint kill");
        assert_eq!(
            store
                .mail_rebuild_jobs()
                .await
                .expect("read rebuild jobs after pre-checkpoint kill")
                .len(),
            0,
            "a rebuild job must not appear before its transaction runs"
        );
        assert!(
            store
                .cached_message_content(&message.id)
                .await
                .expect("read cache after pre-checkpoint kill")
                .is_none(),
            "message content must not appear before its cache transaction runs"
        );
        let durable = store
            .message(&message.id)
            .await
            .expect("read seed message after pre-checkpoint kill")
            .expect("seed message remains recoverable");
        assert_eq!(durable.subject, message.subject);
        drop(store);
    }

    kill_worker(
        spawn_checkpoint_worker(&database, account.id, &message.id, "rebuild", "after").await,
    )
    .await;
    let store = Store::open(&database)
        .await
        .expect("reopen after committed rebuild job");
    let jobs = store
        .mail_rebuild_jobs()
        .await
        .expect("read committed rebuild job");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].account_id, account.id);
    assert_eq!(jobs[0].phase, "checkpoint-fixture");
    assert_eq!(jobs[0].completed, 7);
    assert_eq!(jobs[0].total, Some(11));
    assert!(jobs[0].reset_before_sync);
    assert!(store
        .cached_message_content(&message.id)
        .await
        .expect("cache remains empty after rebuild commit")
        .is_none());
    drop(store);

    kill_worker(
        spawn_checkpoint_worker(&database, account.id, &message.id, "content-cache", "after").await,
    )
    .await;
    for _ in 0..3 {
        let store = Store::open(&database)
            .await
            .expect("reopen after committed cache write");
        let cached = store
            .cached_message_content(&message.id)
            .await
            .expect("read committed cache entry")
            .expect("complete cache entry survives process termination");
        assert_eq!(cached.body_text, "fictional cached body after checkpoint");
        assert_eq!(
            cached.body_html.as_deref(),
            Some("<p>fictional cached body</p>")
        );
        assert!(cached.attachments.is_empty());
        assert_eq!(
            store
                .mail_rebuild_jobs()
                .await
                .expect("read rebuild job after cache commit")[0]
                .phase,
            "checkpoint-fixture"
        );
        let durable = store
            .message(&message.id)
            .await
            .expect("read message after cache commit")
            .expect("catalogue message survives cache commit");
        assert_eq!(durable.account_id, account.id.to_string());
        drop(store);
    }
}

#[tokio::test]
async fn corrupt_database_fails_without_being_replaced_by_an_empty_profile() {
    let temporary = TempDir::new().expect("temporary directory");
    let profile = temporary.path().join("corrupt-profile");
    std::fs::create_dir_all(&profile).expect("create corrupt profile");
    let database = profile.join("dakia.db");
    let corrupt_bytes = b"Dakia intentionally corrupt SQLite fixture\0\xff\x01";
    std::fs::write(&database, corrupt_bytes).expect("write corrupt database fixture");
    let profile_arg = profile.to_string_lossy().into_owned();

    let output = run(&["--data-dir", &profile_arg, "--json", "account", "list"]).await;
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_ascii_lowercase()
            .contains("database"),
        "failure should identify the database boundary: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&database).expect("read corrupt database after failed open"),
        corrupt_bytes,
        "startup must preserve the damaged source for recovery instead of replacing it"
    );

    let second = run(&["--data-dir", &profile_arg, "--json", "account", "list"]).await;
    assert_ne!(second.status.code(), Some(0));
    assert_eq!(
        std::fs::read(&database).expect("read corrupt database after repeated failure"),
        corrupt_bytes
    );
}

/// Worker half of
/// `cancelling_a_blocking_worker_exits_bounded_without_corrupting_profile_state`.
/// It is a no-op in the ordinary suite; the parent invokes it by exact name
/// with the profile and message supplied in its environment.
#[tokio::test]
async fn cross_process_content_claim_worker() {
    let Ok(database) = std::env::var("DAKIA_CLI_CLAIM_WORKER_DATABASE") else {
        return;
    };
    let message_id = std::env::var("DAKIA_CLI_CLAIM_WORKER_MESSAGE_ID")
        .expect("worker message id is configured");
    let hold_millis = std::env::var("DAKIA_CLI_CLAIM_WORKER_HOLD_MILLIS")
        .expect("worker hold duration is configured")
        .parse::<u64>()
        .expect("worker hold duration is an integer");
    let store = Store::open(database).await.expect("worker opens profile");

    assert!(
        store
            .claim_message_content_fetch(&message_id)
            .await
            .expect("worker claims message content"),
        "worker owns the initial content-fetch lease"
    );
    println!("{CLAIM_WORKER_READY}");
    sleep(Duration::from_millis(hold_millis)).await;
}

/// Worker half of
/// `process_kills_before_and_after_a_store_transaction_preserve_only_committed_state`.
#[tokio::test]
async fn cross_process_transaction_worker() {
    let Ok(database) = std::env::var("DAKIA_CLI_TRANSACTION_WORKER_DATABASE") else {
        return;
    };
    let account_id = std::env::var("DAKIA_CLI_TRANSACTION_WORKER_ACCOUNT_ID")
        .expect("transaction worker account id")
        .parse::<AccountId>()
        .expect("valid transaction worker account id");
    let phase =
        std::env::var("DAKIA_CLI_TRANSACTION_WORKER_PHASE").expect("transaction worker phase");
    let hold_millis = std::env::var("DAKIA_CLI_TRANSACTION_WORKER_HOLD_MILLIS")
        .expect("transaction worker hold duration")
        .parse::<u64>()
        .expect("transaction worker hold duration is an integer");
    let store = Store::open(database)
        .await
        .expect("transaction worker opens profile");

    if phase == "before" {
        println!("{TRANSACTION_WORKER_READY}");
        sleep(Duration::from_millis(hold_millis)).await;
        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(99))
            .await
            .expect("pre-kill worker move");
    } else if phase == "after" {
        store
            .move_message(account_id, "INBOX", 1, "Archive", Some(99))
            .await
            .expect("post-commit worker move");
        println!("{TRANSACTION_WORKER_READY}");
        sleep(Duration::from_millis(hold_millis)).await;
    } else {
        panic!("unknown transaction worker phase: {phase}");
    }
}

/// Worker half of
/// `process_kills_around_rebuild_and_content_cache_commits_reopen_only_complete_state`.
#[tokio::test]
async fn cross_process_checkpoint_worker() {
    let Ok(database) = std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_DATABASE") else {
        return;
    };
    let account_id = std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_ACCOUNT_ID")
        .expect("checkpoint worker account id")
        .parse::<AccountId>()
        .expect("valid checkpoint worker account id");
    let message_id = std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_MESSAGE_ID")
        .expect("checkpoint worker message id");
    let operation = std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_OPERATION")
        .expect("checkpoint worker operation");
    let phase =
        std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_PHASE").expect("checkpoint worker phase");
    let hold_millis = std::env::var("DAKIA_CLI_CHECKPOINT_WORKER_HOLD_MILLIS")
        .expect("checkpoint worker hold duration")
        .parse::<u64>()
        .expect("checkpoint worker hold duration is an integer");
    let store = Store::open(database)
        .await
        .expect("checkpoint worker opens profile");

    if phase == "before" {
        println!("{CHECKPOINT_WORKER_READY}");
        sleep(Duration::from_millis(hold_millis)).await;
    }

    match operation.as_str() {
        "rebuild" => store
            .save_mail_rebuild_job(&MailRebuildJob {
                account_id,
                phase: "checkpoint-fixture".into(),
                completed: 7,
                total: Some(11),
                reset_before_sync: true,
            })
            .await
            .expect("checkpoint worker commits rebuild job"),
        "content-cache" => store
            .cache_message_content(
                &message_id,
                false,
                CachedMessageContent {
                    body_text: "fictional cached body after checkpoint".into(),
                    body_html: Some("<p>fictional cached body</p>".into()),
                    unsubscribe_kind: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("checkpoint worker commits content cache"),
        _ => panic!("unknown checkpoint worker operation: {operation}"),
    }

    if phase == "after" {
        println!("{CHECKPOINT_WORKER_READY}");
        sleep(Duration::from_millis(hold_millis)).await;
    } else if phase != "before" {
        panic!("unknown checkpoint worker phase: {phase}");
    }
}
