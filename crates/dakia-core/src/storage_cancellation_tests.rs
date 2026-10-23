use super::*;
use crate::AccountDraft;
use std::path::Path;
use tempfile::tempdir;
use tokio::{sync::oneshot, time::timeout};

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);

fn profile_file_descriptor_count(profile: &Path) -> Option<usize> {
    let profile = profile.canonicalize().ok()?;
    let descriptors = [Path::new("/dev/fd"), Path::new("/proc/self/fd")]
        .into_iter()
        .find_map(|path| std::fs::read_dir(path).ok())?;
    Some(
        descriptors
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(&profile))
            .count(),
    )
}

fn file_backed_account() -> Account {
    AccountDraft {
        email: "cancelled-fetch@example.test".into(),
        display_name: "Cancelled fetch".into(),
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
    .into_account(provider::by_id("fastmail").expect("fixture provider"))
}

fn file_backed_message(account: &Account) -> MailSummary {
    MailSummary {
        id: stable_message_id(account.id, "INBOX", 1),
        account_id: account.id.to_string(),
        mailbox: "INBOX".into(),
        uid: 1,
        mailbox_uid_validity: Some(77),
        message_id: Some("<cancelled-fetch@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "cancelled-fetch@example.test".into(),
        subject: "Cancellation fixture".into(),
        from_name: Some("Fictional Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: account.email.clone(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: "file-backed cancellation".into(),
        body_text: String::new(),
        body_html: None,
        content_state: "headers_only".into(),
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
async fn cancellation_with_a_checked_out_file_connection_cleans_the_eventual_claim() {
    const CANCELLATION_CYCLES: usize = 25;
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("cancelled-fetch.sqlite3");
    let store = Store::open(&database)
        .await
        .expect("open file-backed store");
    let account = file_backed_account();
    let message = file_backed_message(&account);
    store.save_account(&account).await.expect("save account");
    store
        .upsert_catalog_messages(std::slice::from_ref(&message))
        .await
        .expect("save message header");
    let steady_descriptors = profile_file_descriptor_count(directory.path())
        .expect("platform must expose process file descriptors");

    for cycle in 0..CANCELLATION_CYCLES {
        let owner_store = store.clone();
        let owner_message_id = message.id.clone();
        let (checked_out, wait_until_checked_out) = oneshot::channel();
        let (resume, wait_to_resume) = oneshot::channel();
        let (released, wait_until_released) = oneshot::channel();
        let owner = tokio::spawn(async move {
            owner_store
                .acquire_message_content_fetch_with_checkout_barrier(
                    &owner_message_id,
                    checked_out,
                    wait_to_resume,
                    released,
                )
                .await
        });

        timeout(BARRIER_TIMEOUT, wait_until_checked_out)
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle} never checked out the SQLite connection"))
            .expect("checkout barrier sender dropped");
        assert!(
            store.pool.try_acquire().is_none(),
            "cycle {cycle} must cancel while the only file-backed connection is checked out"
        );

        owner.abort();
        let cancelled = timeout(BARRIER_TIMEOUT, owner)
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle} owner cancellation timed out"));
        match cancelled {
            Err(error) => assert!(
                error.is_cancelled(),
                "cycle {cycle} must end by cancellation: {error}"
            ),
            Ok(_) => panic!("cycle {cycle} owner completed instead of being cancelled"),
        }

        // The owned acquisition task is intentionally detached from the
        // cancelled caller. Let it finish the SQLite write, construct the RAII
        // claim, and drop that unobserved result. The observer fires only after
        // the resulting claim row has been deleted through production cleanup.
        resume.send(()).expect("resume detached acquisition task");
        timeout(BARRIER_TIMEOUT, wait_until_released)
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle} claim cleanup timed out"))
            .expect("drop cleanup observer disappeared")
            .unwrap_or_else(|error| panic!("cycle {cycle} claim cleanup failed: {error:#}"));

        let remaining_claims: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_content_fetches WHERE message_id = ?")
                .bind(&message.id)
                .fetch_one(&store.pool)
                .await
                .expect("verify claim cleanup and connection reuse");
        assert_eq!(remaining_claims, 0, "cycle {cycle} stranded a claim");
        assert!(
            store
                .account(account.id)
                .await
                .expect("reuse store")
                .is_some(),
            "cycle {cycle} must return the SQLite connection to the pool"
        );
    }

    let descriptors_after = profile_file_descriptor_count(directory.path())
        .expect("platform must expose process file descriptors");
    assert!(
        descriptors_after <= steady_descriptors,
        "repeated cancellation retained profile file descriptors: before={steady_descriptors}, after={descriptors_after}"
    );

    store.pool.close().await;
    drop(store);
    assert_eq!(
        profile_file_descriptor_count(directory.path()),
        Some(0),
        "closing the cancelled file-backed store must release every profile descriptor"
    );
}

#[tokio::test]
async fn file_backed_claim_acquisition_errors_return_without_retrying() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("closed-pool.sqlite3");
    let store = Store::open(&database)
        .await
        .expect("open file-backed store");
    store.pool.close().await;

    let result = timeout(
        BARRIER_TIMEOUT,
        store.acquire_message_content_fetch("missing-after-close"),
    )
    .await
    .expect("closed-pool acquisition must return instead of retrying");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("closed-pool acquisition unexpectedly succeeded"),
    };
    assert!(
        error.to_string().to_ascii_lowercase().contains("closed"),
        "unexpected closed-pool error: {error:#}"
    );
}

#[tokio::test]
async fn error_after_committed_claim_runs_owner_scoped_cleanup() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("post-execute-error.sqlite3");
    let store = Store::open(&database)
        .await
        .expect("open file-backed store");
    let account = file_backed_account();
    let message = file_backed_message(&account);
    store.save_account(&account).await.expect("save account");
    store
        .upsert_catalog_messages(std::slice::from_ref(&message))
        .await
        .expect("save message header");

    let (released, wait_until_released) = oneshot::channel();
    let result = store
        .acquire_message_content_fetch_with_post_execute_error(&message.id, released)
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("injected post-execute error unexpectedly succeeded"),
    };
    assert!(
        error.to_string().contains("injected error after committed"),
        "unexpected injected error: {error:#}"
    );
    timeout(BARRIER_TIMEOUT, wait_until_released)
        .await
        .expect("post-execute cleanup timed out")
        .expect("post-execute cleanup observer disappeared")
        .expect("post-execute owner-scoped cleanup failed");

    let remaining_claims: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_content_fetches WHERE message_id = ?")
            .bind(&message.id)
            .fetch_one(&store.pool)
            .await
            .expect("inspect post-execute claim cleanup");
    assert_eq!(remaining_claims, 0, "post-execute error stranded a claim");
}
