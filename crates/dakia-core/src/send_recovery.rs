use super::{
    send_journal::{
        claim_send_attempt_in_transaction, complete_send_attempt_in_transaction,
        release_send_attempt_in_transaction,
    },
    SendAttemptClaim, SendAttemptStatus, Store,
};
use crate::{AccountId, ComposeMessage};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, QueryBuilder, Sqlite};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The largest serialized compose payload retained for a recoverable send.
///
/// Outbound validation independently caps attachments at 50 MiB. This bound
/// leaves room for base64 transport overhead and the non-attachment fields,
/// while preventing a damaged profile from making recovery enumeration load an
/// unbounded row.
const MAX_SEND_RECOVERY_PAYLOAD_BYTES: usize = 72 * 1024 * 1024;
/// Full subjects remain in the account-bound payload. This independent list
/// field is capped so a page cannot allocate draft-sized subject strings.
const MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES: usize = 4 * 1024;
const SEND_RECOVERY_PAGE_SIZE: usize = 100;

/// The only states a recovery UI may expose. `NotSubmitted` means the prior
/// submission was definitely not sent. Every other state is intentionally
/// view-only because SMTP may have accepted the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendRecoveryStatus {
    NotSubmitted,
    Pending,
    DeliveryUncertain,
    SentCopyFailed,
}

/// Result of durably preparing an outbound message before SMTP may be polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRecoveryClaim {
    Claimed,
    Existing(SendRecoveryStatus),
    AlreadySubmitted,
    Mismatch,
}

/// A stable opaque pagination position for account-scoped recovery summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendRecoveryCursor {
    pub updated_at: DateTime<Utc>,
    pub account_id: AccountId,
    pub attempt_id: Uuid,
}

/// Content-light recovery metadata for an Outbox or account-level list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SendRecoverySummary {
    pub account_id: AccountId,
    pub attempt_id: Uuid,
    pub status: SendRecoveryStatus,
    pub subject: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SendRecoveryPage {
    pub items: Vec<SendRecoverySummary>,
    pub next_cursor: Option<SendRecoveryCursor>,
    pub total_count: u64,
}

/// The account-bound payload returned only after the bridge validates a
/// recovery-window grant. The payload is never embedded in a window URL.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendRecovery {
    pub account_id: AccountId,
    pub attempt_id: Uuid,
    pub status: SendRecoveryStatus,
    pub message: ComposeMessage,
}

#[derive(FromRow)]
struct RecoveryRow {
    account_id: String,
    attempt_id: String,
    payload_hash: String,
    payload_json: Vec<u8>,
    retryable: i64,
    journal_status: Option<String>,
    journal_payload_hash: Option<String>,
}

#[derive(FromRow)]
struct RecoverySummaryRow {
    account_id: String,
    attempt_id: String,
    payload_hash: String,
    retryable: i64,
    subject: String,
    updated_at: DateTime<Utc>,
    journal_status: Option<String>,
    journal_payload_hash: Option<String>,
}

fn payload_hash(payload: &[u8]) -> String {
    format!("{:x}", Sha256::digest(payload))
}

fn canonical_payload(message: &ComposeMessage) -> Result<(Vec<u8>, String)> {
    let payload = serde_json::to_vec(message).context("serializing recoverable compose payload")?;
    if payload.len() > MAX_SEND_RECOVERY_PAYLOAD_BYTES {
        bail!(
            "recoverable compose payload exceeds the {} MiB safety limit",
            MAX_SEND_RECOVERY_PAYLOAD_BYTES / 1024 / 1024
        );
    }
    let hash = payload_hash(&payload);
    Ok((payload, hash))
}

fn display_subject(subject: &str) -> String {
    if subject.len() <= MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES {
        return subject.to_owned();
    }
    let mut end = MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES;
    while !subject.is_char_boundary(end) {
        end -= 1;
    }
    subject[..end].to_owned()
}

fn parse_id(value: &str, name: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("persisted send recovery {name} is invalid"))
}

fn decode_recovery_message(row: &RecoveryRow) -> Result<(AccountId, Uuid, ComposeMessage)> {
    if row.payload_json.len() > MAX_SEND_RECOVERY_PAYLOAD_BYTES {
        bail!("persisted recoverable compose payload exceeds the safety limit");
    }
    if payload_hash(&row.payload_json) != row.payload_hash {
        bail!("persisted recoverable compose payload hash does not match");
    }
    let account_id = parse_id(&row.account_id, "account ID")?;
    let attempt_id = parse_id(&row.attempt_id, "attempt ID")?;
    let message: ComposeMessage = serde_json::from_slice(&row.payload_json)
        .context("persisted recoverable compose payload is invalid")?;
    if message.account_id != account_id {
        bail!("persisted recoverable compose payload belongs to another account");
    }
    Ok((account_id, attempt_id, message))
}

fn recovery_status(row: &RecoveryRow) -> Result<SendRecoveryStatus> {
    recovery_status_fields(
        row.retryable,
        &row.payload_hash,
        row.journal_status.as_deref(),
        row.journal_payload_hash.as_deref(),
    )
}

fn recovery_status_fields(
    retryable: i64,
    payload_hash: &str,
    journal_status: Option<&str>,
    journal_payload_hash: Option<&str>,
) -> Result<SendRecoveryStatus> {
    if let Some(journal_payload_hash) = journal_payload_hash {
        if journal_payload_hash != payload_hash {
            bail!("send recovery payload does not match its send-attempt claim");
        }
    }
    match (retryable, journal_status) {
        (1, None) => Ok(SendRecoveryStatus::NotSubmitted),
        (0, Some("pending")) => Ok(SendRecoveryStatus::Pending),
        (0, Some("delivery_uncertain")) => Ok(SendRecoveryStatus::DeliveryUncertain),
        (0, Some("sent_copy_failed")) => Ok(SendRecoveryStatus::SentCopyFailed),
        (0, Some("submitted")) => bail!("submitted send recovery payload was not cleaned up"),
        (0, None) => bail!("send recovery payload is missing its send-attempt claim"),
        (1, Some(_)) => bail!("retryable send recovery payload still has a send-attempt claim"),
        (_, Some(_)) => bail!("persisted send recovery state is invalid"),
        (_, None) => bail!("persisted send recovery state is invalid"),
    }
}

fn summary_from_row(row: &RecoverySummaryRow) -> Result<SendRecoverySummary> {
    let account_id = parse_id(&row.account_id, "account ID")?;
    let attempt_id = parse_id(&row.attempt_id, "attempt ID")?;
    Ok(SendRecoverySummary {
        account_id,
        attempt_id,
        status: recovery_status_fields(
            row.retryable,
            &row.payload_hash,
            row.journal_status.as_deref(),
            row.journal_payload_hash.as_deref(),
        )?,
        subject: row.subject.clone(),
        updated_at: row.updated_at,
    })
}

impl Store {
    /// Persists the exact canonical compose JSON and claims its account-scoped
    /// send attempt in one SQLite transaction. Call this before waiting on an
    /// account-operation lock or polling an SMTP future.
    pub async fn prepare_send_recovery(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        message: &ComposeMessage,
    ) -> Result<SendRecoveryClaim> {
        if message.account_id != account_id {
            bail!("recoverable compose payload belongs to another account");
        }
        let (payload_json, payload_hash) = canonical_payload(message)?;
        let display_subject = display_subject(&message.subject);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self
                .prepare_send_recovery_once(
                    account_id,
                    attempt_id,
                    &payload_json,
                    &payload_hash,
                    &display_subject,
                )
                .await
            {
                Err(error) if super::is_sqlite_busy(&error) && Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                result => return result,
            }
        }
    }

    async fn prepare_send_recovery_once(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        payload_json: &[u8],
        payload_hash: &str,
        display_subject: &str,
    ) -> Result<SendRecoveryClaim> {
        let account = account_id.to_string();
        let attempt = attempt_id.to_string();
        let now = Utc::now();
        let mut transaction = self.pool.begin().await?;

        let recovery: Option<(String, i64)> = sqlx::query_as(
            "SELECT payload_hash, retryable FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ?",
        )
        .bind(&account)
        .bind(&attempt)
        .fetch_optional(&mut *transaction)
        .await
        .context("reading existing send recovery payload")?;
        let journal: Option<(String, String)> = sqlx::query_as(
            "SELECT payload_hash, status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
        )
        .bind(&account)
        .bind(&attempt)
        .fetch_optional(&mut *transaction)
        .await
        .context("reading existing send recovery claim")?;

        match recovery {
            Some((existing_hash, retryable)) => {
                match (retryable, journal.as_ref()) {
                    (1, None) => {
                        // This is the one safe same-ID edit case: a previous
                        // NotSubmitted result removed its journal claim, so no
                        // SMTP outcome can belong to the old payload. Replace
                        // the exact stored bytes and hash before claiming the
                        // edited message in this same transaction.
                        sqlx::query(
                            "UPDATE send_recovery_payloads SET payload_hash = ?, payload_json = ?, subject = ?, retryable = 0, updated_at = ? WHERE account_id = ? AND attempt_id = ? AND payload_hash = ? AND retryable = 1",
                        )
                        .bind(payload_hash)
                        .bind(payload_json)
                        .bind(display_subject)
                        .bind(now)
                        .bind(&account)
                        .bind(&attempt)
                        .bind(&existing_hash)
                        .execute(&mut *transaction)
                        .await
                        .context("claiming retryable send recovery payload")?;
                    }
                    (0, Some((journal_hash, _))) if journal_hash == &existing_hash => {
                        if existing_hash != payload_hash {
                            return Ok(SendRecoveryClaim::Mismatch);
                        }
                    }
                    (0, None) => {
                        bail!("send recovery payload is missing its send-attempt claim");
                    }
                    (1, Some(_)) => {
                        bail!("retryable send recovery payload still has a send-attempt claim");
                    }
                    _ => return Ok(SendRecoveryClaim::Mismatch),
                }
            }
            None => {
                if let Some((journal_hash, journal_status)) = journal.as_ref() {
                    if journal_hash != payload_hash {
                        return Ok(SendRecoveryClaim::Mismatch);
                    }
                    if journal_status == "submitted" {
                        return Ok(SendRecoveryClaim::AlreadySubmitted);
                    }
                }
                sqlx::query(
                    "INSERT INTO send_recovery_payloads(account_id, attempt_id, payload_hash, payload_json, subject, retryable, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
                )
                .bind(&account)
                .bind(&attempt)
                .bind(payload_hash)
                .bind(payload_json)
                .bind(display_subject)
                .bind(now)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .context("creating send recovery payload")?;
            }
        }

        let claim = claim_send_attempt_in_transaction(
            &mut transaction,
            account_id,
            attempt_id,
            payload_hash,
        )
        .await?;
        let result = match claim {
            SendAttemptClaim::Claimed => SendRecoveryClaim::Claimed,
            SendAttemptClaim::Mismatch => {
                transaction.rollback().await?;
                return Ok(SendRecoveryClaim::Mismatch);
            }
            SendAttemptClaim::Existing(SendAttemptStatus::Submitted) => {
                // This can happen only for a pre-recovery journal row. Do not
                // create a fresh editable copy of something already submitted.
                transaction.rollback().await?;
                return Ok(SendRecoveryClaim::AlreadySubmitted);
            }
            SendAttemptClaim::Existing(SendAttemptStatus::Pending) => {
                SendRecoveryClaim::Existing(SendRecoveryStatus::Pending)
            }
            SendAttemptClaim::Existing(SendAttemptStatus::DeliveryUncertain) => {
                SendRecoveryClaim::Existing(SendRecoveryStatus::DeliveryUncertain)
            }
            SendAttemptClaim::Existing(SendAttemptStatus::SentCopyFailed) => {
                SendRecoveryClaim::Existing(SendRecoveryStatus::SentCopyFailed)
            }
        };
        transaction.commit().await?;
        Ok(result)
    }

    /// Atomically records an SMTP outcome against a prepared recovery record.
    /// A submitted outcome removes the payload only after the journal outcome
    /// is durable; other outcomes remain view-only recovery items.
    pub async fn finish_send_recovery(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        status: SendAttemptStatus,
    ) -> Result<()> {
        if status == SendAttemptStatus::Pending {
            bail!("cannot finish send recovery as pending");
        }
        let account = account_id.to_string();
        let attempt = attempt_id.to_string();
        let mut transaction = self.pool.begin().await?;
        let recovery: Option<(String, i64)> = sqlx::query_as(
            "SELECT payload_hash, retryable FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ?",
        )
        .bind(&account)
        .bind(&attempt)
        .fetch_optional(&mut *transaction)
        .await
        .context("reading recovery payload before finalization")?;
        let Some((payload_hash, retryable)) = recovery else {
            if status == SendAttemptStatus::Submitted {
                let existing: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
                )
                .bind(&account)
                .bind(&attempt)
                .fetch_optional(&mut *transaction)
                .await?;
                if existing.as_deref() == Some("submitted") {
                    transaction.commit().await?;
                    return Ok(());
                }
            }
            bail!("send recovery payload does not exist");
        };
        if retryable != 0 {
            bail!("cannot finish a retryable send recovery payload");
        }
        complete_send_attempt_in_transaction(
            &mut transaction,
            account_id,
            attempt_id,
            &payload_hash,
            status,
        )
        .await?;
        if status == SendAttemptStatus::Submitted {
            sqlx::query(
                "DELETE FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ? AND payload_hash = ?",
            )
            .bind(&account)
            .bind(&attempt)
            .bind(&payload_hash)
            .execute(&mut *transaction)
            .await
            .context("removing submitted recovery payload")?;
        } else {
            sqlx::query(
                "UPDATE send_recovery_payloads SET retryable = 0, updated_at = ? WHERE account_id = ? AND attempt_id = ? AND payload_hash = ?",
            )
            .bind(Utc::now())
            .bind(&account)
            .bind(&attempt)
            .bind(&payload_hash)
            .execute(&mut *transaction)
            .await
            .context("recording terminal send recovery state")?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Releases a prepared record only after a definite NotSubmitted result.
    /// The journal claim disappears and the original canonical payload becomes
    /// user-retryable in the same transaction.
    pub async fn release_send_recovery(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
    ) -> Result<()> {
        let account = account_id.to_string();
        let attempt = attempt_id.to_string();
        let mut transaction = self.pool.begin().await?;
        let recovery: Option<(String, i64)> = sqlx::query_as(
            "SELECT payload_hash, retryable FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ?",
        )
        .bind(&account)
        .bind(&attempt)
        .fetch_optional(&mut *transaction)
        .await
        .context("reading recovery payload before release")?;
        let Some((payload_hash, retryable)) = recovery else {
            bail!("send recovery payload does not exist");
        };
        if retryable != 0 {
            bail!("send recovery payload is already retryable");
        }
        release_send_attempt_in_transaction(
            &mut transaction,
            account_id,
            attempt_id,
            &payload_hash,
        )
        .await?;
        sqlx::query(
            "UPDATE send_recovery_payloads SET retryable = 1, updated_at = ? WHERE account_id = ? AND attempt_id = ? AND payload_hash = ?",
        )
        .bind(Utc::now())
        .bind(&account)
        .bind(&attempt)
        .bind(&payload_hash)
        .execute(&mut *transaction)
        .await
        .context("marking send recovery payload retryable")?;
        transaction.commit().await?;
        Ok(())
    }

    /// Lists at most 100 account-scoped recovery summaries. The cursor is an
    /// exclusive keyset cursor, so old items remain reachable without relying
    /// on offset pagination while new recovery attempts arrive.
    pub async fn list_send_recoveries(
        &self,
        account_ids: &[AccountId],
        cursor: Option<&SendRecoveryCursor>,
    ) -> Result<SendRecoveryPage> {
        if account_ids.is_empty() {
            return Ok(SendRecoveryPage {
                items: Vec::new(),
                next_cursor: None,
                total_count: 0,
            });
        }
        let mut count_query = QueryBuilder::<Sqlite>::new(
            "SELECT COUNT(*) FROM send_recovery_payloads WHERE account_id IN (",
        );
        {
            let mut separated = count_query.separated(", ");
            for account_id in account_ids {
                separated.push_bind(account_id.to_string());
            }
        }
        count_query.push(")");
        let total_count: i64 = count_query
            .build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .context("counting send recovery payloads")?;
        let total_count = u64::try_from(total_count)
            .map_err(|_| anyhow!("persisted send recovery count is invalid"))?;
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT r.account_id, r.attempt_id, r.payload_hash, r.retryable, r.subject, r.updated_at, j.status AS journal_status, j.payload_hash AS journal_payload_hash FROM send_recovery_payloads r LEFT JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id IN (",
        );
        {
            let mut separated = query.separated(", ");
            for account_id in account_ids {
                separated.push_bind(account_id.to_string());
            }
        }
        query.push(")");
        if let Some(cursor) = cursor {
            query.push(" AND (r.updated_at < ");
            query.push_bind(cursor.updated_at);
            query.push(" OR (r.updated_at = ");
            query.push_bind(cursor.updated_at);
            query.push(" AND (r.account_id > ");
            query.push_bind(cursor.account_id.to_string());
            query.push(" OR (r.account_id = ");
            query.push_bind(cursor.account_id.to_string());
            query.push(" AND r.attempt_id > ");
            query.push_bind(cursor.attempt_id.to_string());
            query.push(") )))");
        }
        query.push(" ORDER BY r.updated_at DESC, r.account_id ASC, r.attempt_id ASC LIMIT ");
        query.push_bind((SEND_RECOVERY_PAGE_SIZE + 1) as i64);
        let mut rows: Vec<RecoverySummaryRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .context("listing send recovery payloads")?;
        let has_more = rows.len() > SEND_RECOVERY_PAGE_SIZE;
        rows.truncate(SEND_RECOVERY_PAGE_SIZE);
        let items = rows
            .iter()
            .map(summary_from_row)
            .collect::<Result<Vec<_>>>()?;
        let next_cursor = has_more.then(|| {
            let item = items.last().expect("a full recovery page has an item");
            SendRecoveryCursor {
                updated_at: item.updated_at,
                account_id: item.account_id,
                attempt_id: item.attempt_id,
            }
        });
        Ok(SendRecoveryPage {
            items,
            next_cursor,
            total_count,
        })
    }

    /// Loads one fresh recovery payload after the native bridge has bound the
    /// target account and attempt to a compose child window.
    pub async fn load_send_recovery(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
    ) -> Result<Option<SendRecovery>> {
        let row: Option<RecoveryRow> = sqlx::query_as(
            "SELECT r.account_id, r.attempt_id, r.payload_hash, r.payload_json, r.retryable, r.updated_at, j.status AS journal_status, j.payload_hash AS journal_payload_hash FROM send_recovery_payloads r LEFT JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id WHERE r.account_id = ? AND r.attempt_id = ?",
        )
        .bind(account_id.to_string())
        .bind(attempt_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .context("loading send recovery payload")?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (stored_account_id, stored_attempt_id, message) = decode_recovery_message(&row)?;
        if stored_account_id != account_id || stored_attempt_id != attempt_id {
            bail!("persisted send recovery target does not match the requested account");
        }
        Ok(Some(SendRecovery {
            account_id,
            attempt_id,
            status: recovery_status(&row)?,
            message,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{SendRecoveryClaim, SendRecoveryStatus, Store, SEND_RECOVERY_PAGE_SIZE};
    use crate::{mail::ComposeAttachment, provider, AccountDraft, ComposeMessage};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Barrier;
    use uuid::Uuid;

    async fn save_test_account(store: &Store, id: Uuid, email: &str) {
        let mut account = AccountDraft {
            email: email.into(),
            display_name: "Send recovery test".into(),
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
        store.save_account(&account).await.unwrap();
    }

    fn draft(account_id: Uuid, subject: impl Into<String>) -> ComposeMessage {
        ComposeMessage {
            account_id,
            to: vec!["recipient@example.test".into()],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            body_text: "Recover this fictional message".into(),
            body_html: Some("<p>Recover this fictional message</p>".into()),
            in_reply_to: None,
            references: None,
            attachments: vec![ComposeAttachment {
                filename: "fictional.txt".into(),
                mime_type: "text/plain".into(),
                content_base64: "ZmljdGlvbmFs".into(),
            }],
        }
    }

    #[tokio::test]
    async fn prepared_payload_survives_a_crash_barrier_and_reopen_without_releasing_pending_send() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recovery.sqlite");
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let message = draft(account_id, "Fictional pending recovery");
        let store = Store::open(&path).await.unwrap();
        save_test_account(&store, account_id, "recovery@example.test").await;
        assert_eq!(
            store
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
                .unwrap(),
            SendRecoveryClaim::Claimed
        );
        drop(store);

        let reopened = Store::open(&path).await.unwrap();
        let recovery = reopened
            .load_send_recovery(account_id, attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovery.status, SendRecoveryStatus::Pending);
        assert_eq!(
            serde_json::to_vec(&recovery.message).unwrap(),
            serde_json::to_vec(&message).unwrap()
        );
        assert_eq!(
            reopened
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
                .unwrap(),
            SendRecoveryClaim::Existing(SendRecoveryStatus::Pending)
        );
    }

    #[tokio::test]
    async fn concurrent_prepare_claims_exactly_once_and_never_leaves_payload_without_journal() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("concurrent-recovery.sqlite");
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let message = draft(account_id, "Concurrent recovery");
        let first = Store::open(&path).await.unwrap();
        save_test_account(&first, account_id, "concurrent@example.test").await;
        let second = Store::open(&path).await.unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let one = tokio::spawn({
            let store = first.clone();
            let message = message.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                store
                    .prepare_send_recovery(account_id, attempt_id, &message)
                    .await
            }
        });
        let two = tokio::spawn({
            let store = second.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                store
                    .prepare_send_recovery(account_id, attempt_id, &message)
                    .await
            }
        });
        let outcomes = [one.await.unwrap().unwrap(), two.await.unwrap().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == SendRecoveryClaim::Claimed)
                .count(),
            1
        );
        assert!(outcomes.iter().any(|outcome| {
            *outcome == SendRecoveryClaim::Existing(SendRecoveryStatus::Pending)
        }));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_recovery_payloads r JOIN send_attempts j ON j.account_id = r.account_id AND j.attempt_id = r.attempt_id")
            .fetch_one(&first.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn not_submitted_payload_can_be_atomically_edited_with_the_same_attempt_id() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let message = draft(account_id, "First text");
        save_test_account(&store, account_id, "retry@example.test").await;
        assert_eq!(
            store
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
                .unwrap(),
            SendRecoveryClaim::Claimed
        );
        store
            .release_send_recovery(account_id, attempt_id)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_send_recovery(account_id, attempt_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SendRecoveryStatus::NotSubmitted
        );
        let edited = draft(account_id, "Changed text");
        assert_eq!(
            store
                .prepare_send_recovery(account_id, attempt_id, &edited)
                .await
                .unwrap(),
            SendRecoveryClaim::Claimed
        );
        assert_eq!(
            store
                .load_send_recovery(account_id, attempt_id)
                .await
                .unwrap()
                .unwrap()
                .message
                .subject,
            "Changed text"
        );
        assert_eq!(
            store
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
                .unwrap(),
            SendRecoveryClaim::Mismatch
        );
    }

    #[tokio::test]
    async fn concurrent_old_and_edited_retries_claim_at_most_one_payload() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("concurrent-edit.sqlite");
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let original = draft(account_id, "Original retry");
        let edited = draft(account_id, "Edited retry");
        let first = Store::open(&path).await.unwrap();
        save_test_account(&first, account_id, "concurrent-edit@example.test").await;
        first
            .prepare_send_recovery(account_id, attempt_id, &original)
            .await
            .unwrap();
        first
            .release_send_recovery(account_id, attempt_id)
            .await
            .unwrap();
        let second = Store::open(&path).await.unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let old = tokio::spawn({
            let store = first.clone();
            let barrier = barrier.clone();
            let original = original.clone();
            async move {
                barrier.wait().await;
                store
                    .prepare_send_recovery(account_id, attempt_id, &original)
                    .await
            }
        });
        let new = tokio::spawn({
            let barrier = barrier.clone();
            let edited = edited.clone();
            async move {
                barrier.wait().await;
                second
                    .prepare_send_recovery(account_id, attempt_id, &edited)
                    .await
            }
        });
        let outcomes = [old.await.unwrap().unwrap(), new.await.unwrap().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == SendRecoveryClaim::Claimed)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == SendRecoveryClaim::Mismatch)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_states_are_view_only_and_submitted_payloads_are_removed_atomically() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        save_test_account(&store, account_id, "terminal@example.test").await;
        for (status, expected) in [
            (
                super::SendAttemptStatus::DeliveryUncertain,
                SendRecoveryStatus::DeliveryUncertain,
            ),
            (
                super::SendAttemptStatus::SentCopyFailed,
                SendRecoveryStatus::SentCopyFailed,
            ),
        ] {
            let attempt_id = Uuid::new_v4();
            let message = draft(account_id, format!("{expected:?}"));
            store
                .prepare_send_recovery(account_id, attempt_id, &message)
                .await
                .unwrap();
            store
                .finish_send_recovery(account_id, attempt_id, status)
                .await
                .unwrap();
            assert_eq!(
                store
                    .load_send_recovery(account_id, attempt_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                expected
            );
            assert_ne!(
                store
                    .prepare_send_recovery(account_id, attempt_id, &message)
                    .await
                    .unwrap(),
                SendRecoveryClaim::Claimed
            );
        }
        let submitted = Uuid::new_v4();
        let message = draft(account_id, "Submitted");
        store
            .prepare_send_recovery(account_id, submitted, &message)
            .await
            .unwrap();
        store
            .finish_send_recovery(account_id, submitted, super::SendAttemptStatus::Submitted)
            .await
            .unwrap();
        assert!(store
            .load_send_recovery(account_id, submitted)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn account_removal_cascades_recovery_payloads_and_paginated_lists_do_not_hide_older_rows()
    {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        save_test_account(&store, account_id, "page@example.test").await;
        for index in 0..=SEND_RECOVERY_PAGE_SIZE {
            store
                .prepare_send_recovery(
                    account_id,
                    Uuid::new_v4(),
                    &draft(account_id, format!("Page {index}")),
                )
                .await
                .unwrap();
        }
        let first = store
            .list_send_recoveries(&[account_id], None)
            .await
            .unwrap();
        assert_eq!(first.items.len(), SEND_RECOVERY_PAGE_SIZE);
        let second = store
            .list_send_recoveries(&[account_id], first.next_cursor.as_ref())
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next_cursor.is_none());
        store.delete_account(account_id).await.unwrap();
        assert!(store
            .list_send_recoveries(&[account_id], None)
            .await
            .unwrap()
            .items
            .is_empty());
    }

    #[tokio::test]
    async fn recovery_list_subject_is_bounded_while_granted_payload_stays_exact() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        save_test_account(&store, account_id, "subject-limit@example.test").await;
        let subject = format!(
            "{}€",
            "x".repeat(super::MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES)
        );
        let message = draft(account_id, subject.clone());
        store
            .prepare_send_recovery(account_id, attempt_id, &message)
            .await
            .unwrap();
        let page = store
            .list_send_recoveries(&[account_id], None)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.items[0].subject.len() <= super::MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES);
        assert!(page.items[0]
            .subject
            .is_char_boundary(page.items[0].subject.len()));
        assert!(
            sqlx::query(
                "UPDATE send_recovery_payloads SET subject = ? WHERE account_id = ? AND attempt_id = ?",
            )
            .bind("x".repeat(super::MAX_SEND_RECOVERY_DISPLAY_SUBJECT_BYTES + 1))
            .bind(account_id.to_string())
            .bind(attempt_id.to_string())
            .execute(&store.pool)
            .await
            .is_err(),
            "the list-summary byte bound must also hold for direct SQLite writes"
        );
        assert_eq!(
            store
                .load_send_recovery(account_id, attempt_id)
                .await
                .unwrap()
                .unwrap()
                .message
                .subject,
            subject
        );
    }

    #[tokio::test]
    async fn corrupt_payload_or_orphaned_claim_is_refused_instead_of_becoming_retryable() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        save_test_account(&store, account_id, "corrupt@example.test").await;
        store
            .prepare_send_recovery(account_id, attempt_id, &draft(account_id, "Corrupt"))
            .await
            .unwrap();
        sqlx::query("UPDATE send_recovery_payloads SET payload_json = X'00' WHERE account_id = ? AND attempt_id = ?")
            .bind(account_id.to_string())
            .bind(attempt_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .load_send_recovery(account_id, attempt_id)
            .await
            .is_err());
        sqlx::query("DELETE FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ?")
            .bind(account_id.to_string())
            .bind(attempt_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let orphan_attempt_id = Uuid::new_v4();
        store
            .prepare_send_recovery(account_id, orphan_attempt_id, &draft(account_id, "Orphan"))
            .await
            .unwrap();
        sqlx::query("DELETE FROM send_attempts WHERE account_id = ? AND attempt_id = ?")
            .bind(account_id.to_string())
            .bind(orphan_attempt_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .list_send_recoveries(&[account_id], None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn recovery_payload_must_match_the_journal_hash_before_it_can_be_presented() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        save_test_account(&store, account_id, "hash-match@example.test").await;
        store
            .prepare_send_recovery(account_id, attempt_id, &draft(account_id, "Hash match"))
            .await
            .unwrap();
        sqlx::query("UPDATE send_attempts SET payload_hash = 'different' WHERE account_id = ? AND attempt_id = ?")
            .bind(account_id.to_string())
            .bind(attempt_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let error = store
            .load_send_recovery(account_id, attempt_id)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its send-attempt claim"),
            "{error:#}"
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::Store;
    use crate::{provider, AccountDraft};
    use tempfile::tempdir;
    use uuid::Uuid;

    async fn save_test_account(store: &Store, id: Uuid) {
        let mut account = AccountDraft {
            email: format!("migration-{id}@example.test"),
            display_name: "Send recovery migration".into(),
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
        store.save_account(&account).await.unwrap();
    }

    #[tokio::test]
    async fn version_one_profile_migrates_to_recovery_schema_without_losing_send_journal() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("version-one.sqlite");
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let store = Store::open(&path).await.unwrap();
        save_test_account(&store, account_id).await;
        store
            .claim_send_attempt(account_id, attempt_id, "existing-journal")
            .await
            .unwrap();
        sqlx::query("DROP TABLE send_recovery_payloads")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE app_meta SET value = '1' WHERE key = 'profile_schema_version'")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;

        let reopened = Store::open(&path).await.unwrap();
        let version: String =
            sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
                .fetch_one(&reopened.pool)
                .await
                .unwrap();
        assert_eq!(version, "3");
        let journal: String = sqlx::query_scalar(
            "SELECT status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
        )
        .bind(account_id.to_string())
        .bind(attempt_id.to_string())
        .fetch_one(&reopened.pool)
        .await
        .unwrap();
        assert_eq!(journal, "pending");
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'send_recovery_payloads')",
        )
        .fetch_one(&reopened.pool)
        .await
        .unwrap();
        assert!(exists);
    }

    #[tokio::test]
    async fn conflicting_recovery_schema_is_refused_before_a_profile_is_rewritten() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("conflicting-recovery.sqlite");
        let store = Store::open(&path).await.unwrap();
        sqlx::query("DROP TABLE send_recovery_payloads")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("CREATE VIEW send_recovery_payloads AS SELECT id FROM accounts")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE app_meta SET value = '1' WHERE key = 'profile_schema_version'")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;

        let error = match Store::open(&path).await {
            Ok(_) => panic!("conflicting recovery schema must be refused"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("send_recovery_payloads conflicts with a non-table schema object"),
            "{error:#}"
        );
        let inspection = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true),
        )
        .await
        .unwrap();
        let target: Option<String> = sqlx::query_scalar(
            "SELECT value FROM app_meta WHERE key = 'profile_schema_migration_target'",
        )
        .fetch_optional(&inspection)
        .await
        .unwrap();
        assert!(
            target.is_none(),
            "a rejected schema must not record a migration target"
        );
    }

    #[tokio::test]
    async fn incomplete_unreleased_version_two_recovery_schema_is_refused() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("incomplete-version-two.sqlite");
        let store = Store::open(&path).await.unwrap();
        sqlx::query("DROP TABLE send_recovery_payloads")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE send_recovery_payloads (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, attempt_id TEXT NOT NULL, payload_hash TEXT NOT NULL, payload_json BLOB NOT NULL, retryable INTEGER NOT NULL DEFAULT 0 CHECK(retryable IN (0, 1)), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, attempt_id))")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;

        let error = match Store::open(&path).await {
            Ok(_) => panic!("incomplete version-two recovery schema must be refused"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("send_recovery_payloads.subject is missing"),
            "{error:#}"
        );
    }
}
