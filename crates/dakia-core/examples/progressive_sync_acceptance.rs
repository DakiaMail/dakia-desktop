use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use dakia_core::{
    provider::Security, storage::OperationTarget, Account, AccountAuth, ComposeMessage,
    MailService, SendOutcome, Store,
};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

fn port(name: &str) -> Result<u16> {
    std::env::var(name)
        .with_context(|| format!("{name} is required"))?
        .parse()
        .with_context(|| format!("{name} must be a port"))
}

fn fixture_account(imap_port: u16, smtp_port: u16) -> Account {
    Account {
        id: Uuid::new_v4(),
        email: "reader@example.test".into(),
        account_name: "Fictional acceptance mailbox".into(),
        display_name: "Fixture Reader".into(),
        provider_id: "custom".into(),
        auth: AccountAuth::Password {
            username: "reader@example.test".into(),
        },
        imap_host: "127.0.0.1".into(),
        imap_port,
        imap_security: Security::Tls,
        smtp_host: "127.0.0.1".into(),
        smtp_port,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    }
}

async fn service() -> Result<(tempfile::TempDir, Store, MailService, Account)> {
    let directory = tempdir()?;
    let store = Store::open(directory.path().join("acceptance.sqlite3")).await?;
    let account = fixture_account(
        port("DAKIA_FIXTURE_IMAP_PORT")?,
        port("DAKIA_FIXTURE_SMTP_PORT")?,
    );
    store.save_account(&account).await?;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "fictional-secret")
        .await?;
    Ok((directory, store, service, account))
}

async fn measure_fresh_window() -> Result<()> {
    let (_directory, store, service, account) = service().await?;
    let expected_messages: usize = std::env::var("DAKIA_FIXTURE_MESSAGES")
        .context("DAKIA_FIXTURE_MESSAGES is required")?
        .parse()
        .context("DAKIA_FIXTURE_MESSAGES must be an integer")?;
    let started = Instant::now();
    let mut first_commit_ms = None;
    let initial = service
        .initial_inbox_with_progress(&account, 50, |progress| {
            if progress.phase == "published" && first_commit_ms.is_none() {
                first_commit_ms = Some(started.elapsed().as_secs_f64() * 1_000.0);
            }
        })
        .await?;
    let discovery_started = Instant::now();
    let mut backfill_rows = 0usize;
    let mut backfill_calls = 0usize;
    let mut header_commit_batches = usize::from(initial.synced_count > 0);
    loop {
        backfill_calls += 1;
        let backfill = service
            .backfill_folder_headers_with_progress(&account, "INBOX", "INBOX", 50, |_| {})
            .await?;
        backfill_rows += backfill.synced_count;
        if backfill.synced_count == 0 {
            break;
        }
        header_commit_batches += 1;
        if backfill_calls > 2_001 {
            anyhow::bail!("historical backfill exceeded its bounded fixture turns");
        }
    }
    let catalogue_rows = store.mailbox_uids(account.id, "INBOX").await?.len();
    if catalogue_rows != expected_messages {
        anyhow::bail!(
            "complete backfill retained {catalogue_rows} rows, expected {expected_messages}"
        );
    }
    println!(
        "DAKIA_LIVE_METRIC {}",
        json!({
            "scenario": "fresh_window",
            "firstCommitMs": first_commit_ms.context("published progress was not emitted")?,
            "initialCommittedRows": initial.synced_count,
            "historicalCommittedRows": backfill_rows,
            "catalogueRows": catalogue_rows,
            "fullBackfillMs": discovery_started.elapsed().as_secs_f64() * 1_000.0,
            "backfillCallsIncludingEmptyCompletion": backfill_calls,
            "headerCommitTransactions": header_commit_batches,
            "mailMetrics": dakia_core::mail_metrics::snapshot(),
        })
    );
    Ok(())
}

async fn verify_smtp_while_history_stalls() -> Result<()> {
    let (_directory, store, service, account) = service().await?;
    let history_service = service.clone();
    let history_account = account.clone();
    let history_started = Instant::now();
    let history = tokio::spawn(async move {
        history_service
            .backfill_folder_headers_with_progress(&history_account, "INBOX", "INBOX", 50, |_| {})
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Fictional concurrent SMTP acceptance".into(),
        body_text: "This message never leaves the loopback fixture.".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    let prepared = service.prepare_outgoing_message(&account, &draft)?;
    let operation = store
        .enqueue_operation(
            account.id,
            "smtp_submission",
            OperationTarget {
                mailbox: None,
                uid: None,
                uid_validity: None,
                message_id: prepared.message_id.as_deref(),
            },
            &serde_json::to_string(&prepared)?,
            None,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    store
        .claim_operation_by_id(&operation.operation_id, "live-acceptance")
        .await?
        .context("SMTP submission was not claimable")?;
    let smtp_started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        service.submit_prepared_smtp(&account, &prepared),
    )
    .await
    .context("SMTP did not finish while historical IMAP remained stalled")??;
    let smtp_ms = smtp_started.elapsed().as_secs_f64() * 1_000.0;
    let history_was_still_running = !history.is_finished();
    let history_result = history.await.context("historical task panicked")??;
    let accepted = matches!(outcome, SendOutcome::Accepted { .. });
    if !accepted || !history_was_still_running {
        anyhow::bail!(
            "SMTP acceptance must finish while the historical IMAP task is still running"
        );
    }
    println!(
        "DAKIA_LIVE_METRIC {}",
        json!({
            "scenario": "smtp_while_history_stalled",
            "smtpAccepted": accepted,
            "smtpMs": smtp_ms,
            "historyWasStillRunningAtSmtpAcceptance": history_was_still_running,
            "historyMs": history_started.elapsed().as_secs_f64() * 1_000.0,
            "historyCommittedRows": history_result.synced_count,
            "mailMetrics": dakia_core::mail_metrics::snapshot(),
        })
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("fresh-window") => measure_fresh_window().await,
        Some("smtp-while-history-stalled") => verify_smtp_while_history_stalls().await,
        _ => anyhow::bail!(
            "usage: progressive_sync_acceptance fresh-window|smtp-while-history-stalled"
        ),
    }
}
