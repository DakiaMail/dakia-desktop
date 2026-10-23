use super::Store;
use crate::AccountId;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use sqlx::{Sqlite, Transaction};
use uuid::Uuid;

/// The durable outcome of a mail submission attempt.
///
/// A pending attempt is deliberately retained across restarts. The caller must
/// determine whether a message was submitted before making a new attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAttemptStatus {
    Pending,
    Submitted,
    DeliveryUncertain,
    SentCopyFailed,
}

impl SendAttemptStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::DeliveryUncertain => "delivery_uncertain",
            Self::SentCopyFailed => "sent_copy_failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "submitted" => Ok(Self::Submitted),
            "delivery_uncertain" => Ok(Self::DeliveryUncertain),
            "sent_copy_failed" => Ok(Self::SentCopyFailed),
            _ => Err(anyhow!("invalid persisted send attempt status")),
        }
    }
}

/// The result of trying to take ownership of a durable send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAttemptClaim {
    Claimed,
    Existing(SendAttemptStatus),
    Mismatch,
}

pub(crate) async fn claim_send_attempt_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    account_id: AccountId,
    attempt_id: Uuid,
    payload_hash: &str,
) -> Result<SendAttemptClaim> {
    let account_id = account_id.to_string();
    let attempt_id = attempt_id.to_string();
    let now = Utc::now();
    let inserted = sqlx::query(
        "INSERT INTO send_attempts(account_id, attempt_id, payload_hash, status, created_at, updated_at) VALUES (?, ?, ?, 'pending', ?, ?) ON CONFLICT(account_id, attempt_id) DO NOTHING",
    )
    .bind(&account_id)
    .bind(&attempt_id)
    .bind(payload_hash)
    .bind(now)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .context("creating send attempt")?;
    if inserted.rows_affected() == 1 {
        return Ok(SendAttemptClaim::Claimed);
    }

    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT payload_hash, status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(&account_id)
    .bind(&attempt_id)
    .fetch_optional(&mut **transaction)
    .await
    .context("reading existing send attempt")?;
    let (existing_hash, status) = existing
        .ok_or_else(|| anyhow!("send attempt disappeared while reading an existing claim"))?;
    if existing_hash != payload_hash {
        return Ok(SendAttemptClaim::Mismatch);
    }
    Ok(SendAttemptClaim::Existing(SendAttemptStatus::parse(
        &status,
    )?))
}

pub(crate) async fn complete_send_attempt_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    account_id: AccountId,
    attempt_id: Uuid,
    payload_hash: &str,
    status: SendAttemptStatus,
) -> Result<()> {
    if status == SendAttemptStatus::Pending {
        return Err(anyhow!("cannot complete a send attempt as pending"));
    }
    let account_id = account_id.to_string();
    let attempt_id = attempt_id.to_string();
    let updated = sqlx::query(
        "UPDATE send_attempts SET status = ?, updated_at = ? WHERE account_id = ? AND attempt_id = ? AND payload_hash = ? AND status = 'pending'",
    )
    .bind(status.as_str())
    .bind(Utc::now())
    .bind(&account_id)
    .bind(&attempt_id)
    .bind(payload_hash)
    .execute(&mut **transaction)
    .await
    .context("finalizing send attempt")?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }

    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT payload_hash, status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(&account_id)
    .bind(&attempt_id)
    .fetch_optional(&mut **transaction)
    .await
    .context("reading send attempt after finalization conflict")?;
    let (existing_hash, existing_status) =
        existing.ok_or_else(|| anyhow!("send attempt does not exist"))?;
    if existing_hash != payload_hash {
        return Err(anyhow!("send attempt payload hash does not match"));
    }
    let existing_status = SendAttemptStatus::parse(&existing_status)?;
    if existing_status == status {
        return Ok(());
    }
    Err(anyhow!("send attempt already has a different outcome"))
}

pub(crate) async fn release_send_attempt_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    account_id: AccountId,
    attempt_id: Uuid,
    payload_hash: &str,
) -> Result<()> {
    let account_id = account_id.to_string();
    let attempt_id = attempt_id.to_string();
    let deleted = sqlx::query(
        "DELETE FROM send_attempts WHERE account_id = ? AND attempt_id = ? AND payload_hash = ? AND status = 'pending'",
    )
    .bind(&account_id)
    .bind(&attempt_id)
    .bind(payload_hash)
    .execute(&mut **transaction)
    .await
    .context("releasing unsubmitted send attempt")?;
    if deleted.rows_affected() == 1 {
        return Ok(());
    }

    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT payload_hash, status FROM send_attempts WHERE account_id = ? AND attempt_id = ?",
    )
    .bind(&account_id)
    .bind(&attempt_id)
    .fetch_optional(&mut **transaction)
    .await
    .context("reading send attempt after release conflict")?;
    let (existing_hash, status) = existing.ok_or_else(|| anyhow!("send attempt does not exist"))?;
    if existing_hash != payload_hash {
        return Err(anyhow!("send attempt payload hash does not match"));
    }
    Err(anyhow!(
        "cannot release a send attempt after it has an outcome ({status})"
    ))
}

impl Store {
    /// Creates an account-scoped send attempt exactly once.
    ///
    /// The insert is atomic. A caller that finds a matching pending or final
    /// attempt must not submit the message again. Reusing an attempt ID with a
    /// different payload hash fails closed with [`SendAttemptClaim::Mismatch`].
    pub async fn claim_send_attempt(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        payload_hash: &str,
    ) -> Result<SendAttemptClaim> {
        let mut transaction = self.pool.begin().await?;
        let claim = claim_send_attempt_in_transaction(
            &mut transaction,
            account_id,
            attempt_id,
            payload_hash,
        )
        .await?;
        transaction.commit().await?;
        Ok(claim)
    }

    /// Finalizes a claimed attempt without allowing a different payload or a
    /// conflicting terminal result to overwrite it. Repeating the same final
    /// result is safe and idempotent. Recovery payload cleanup is committed in
    /// the same transaction as a submitted outcome.
    pub async fn complete_send_attempt(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        payload_hash: &str,
        status: SendAttemptStatus,
    ) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        complete_send_attempt_in_transaction(
            &mut transaction,
            account_id,
            attempt_id,
            payload_hash,
            status,
        )
        .await?;
        let account = account_id.to_string();
        let attempt = attempt_id.to_string();
        if status == SendAttemptStatus::Submitted {
            sqlx::query(
                "DELETE FROM send_recovery_payloads WHERE account_id = ? AND attempt_id = ? AND payload_hash = ?",
            )
            .bind(&account)
            .bind(&attempt)
            .bind(payload_hash)
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
            .bind(payload_hash)
            .execute(&mut *transaction)
            .await
            .context("recording terminal send recovery state")?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Releases a claim only when SMTP was definitely not reached. Pending
    /// attempts are intentionally never released automatically after a crash.
    /// A prepared recovery payload becomes explicitly retryable in the same
    /// transaction that removes the duplicate-send claim.
    pub async fn release_send_attempt(
        &self,
        account_id: AccountId,
        attempt_id: Uuid,
        payload_hash: &str,
    ) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        release_send_attempt_in_transaction(&mut transaction, account_id, attempt_id, payload_hash)
            .await?;
        sqlx::query(
            "UPDATE send_recovery_payloads SET retryable = 1, updated_at = ? WHERE account_id = ? AND attempt_id = ? AND payload_hash = ?",
        )
        .bind(Utc::now())
        .bind(account_id.to_string())
        .bind(attempt_id.to_string())
        .bind(payload_hash)
        .execute(&mut *transaction)
        .await
        .context("marking unsubmitted recovery payload retryable")?;
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SendAttemptClaim, SendAttemptStatus, Store};
    use crate::{provider, AccountDraft};
    use std::sync::Arc;
    use tokio::sync::Barrier;
    use uuid::Uuid;

    async fn save_test_account(store: &Store, id: Uuid, email: &str) {
        let mut account = AccountDraft {
            email: email.into(),
            display_name: "Send journal test".into(),
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
    async fn independent_file_backed_stores_allow_exactly_one_concurrent_claim() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("send-attempts.db");
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let first = Store::open(&database).await.unwrap();
        save_test_account(&first, account_id, "journal@example.test").await;
        let second = Store::open(&database).await.unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let first_task = tokio::spawn({
            let store = first.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                store
                    .claim_send_attempt(account_id, attempt_id, "payload-a")
                    .await
            }
        });
        let second_task = tokio::spawn({
            let store = second.clone();
            async move {
                barrier.wait().await;
                store
                    .claim_send_attempt(account_id, attempt_id, "payload-a")
                    .await
            }
        });

        let first_claim = first_task.await.unwrap().unwrap();
        let second_claim = second_task.await.unwrap().unwrap();
        assert!(matches!(
            first_claim,
            SendAttemptClaim::Claimed | SendAttemptClaim::Existing(SendAttemptStatus::Pending)
        ));
        assert!(matches!(
            second_claim,
            SendAttemptClaim::Claimed | SendAttemptClaim::Existing(SendAttemptStatus::Pending)
        ));
        assert_eq!(
            [first_claim, second_claim]
                .into_iter()
                .filter(|claim| matches!(claim, SendAttemptClaim::Claimed))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reopened_pending_attempt_blocks_resend_and_finalized_outcomes_replay() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("send-attempts.db");
        let account_id = Uuid::new_v4();
        let pending_attempt = Uuid::new_v4();
        let store = Store::open(&database).await.unwrap();
        save_test_account(&store, account_id, "pending@example.test").await;
        assert_eq!(
            store
                .claim_send_attempt(account_id, pending_attempt, "pending-payload")
                .await
                .unwrap(),
            SendAttemptClaim::Claimed
        );
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        assert_eq!(
            reopened
                .claim_send_attempt(account_id, pending_attempt, "pending-payload")
                .await
                .unwrap(),
            SendAttemptClaim::Existing(SendAttemptStatus::Pending)
        );

        for status in [
            SendAttemptStatus::Submitted,
            SendAttemptStatus::DeliveryUncertain,
            SendAttemptStatus::SentCopyFailed,
        ] {
            let attempt_id = Uuid::new_v4();
            reopened
                .claim_send_attempt(account_id, attempt_id, "final-payload")
                .await
                .unwrap();
            reopened
                .complete_send_attempt(account_id, attempt_id, "final-payload", status)
                .await
                .unwrap();
            reopened
                .complete_send_attempt(account_id, attempt_id, "final-payload", status)
                .await
                .unwrap();
            assert_eq!(
                reopened
                    .claim_send_attempt(account_id, attempt_id, "final-payload")
                    .await
                    .unwrap(),
                SendAttemptClaim::Existing(status)
            );
        }
    }

    #[tokio::test]
    async fn payload_mismatch_and_account_scope_cannot_reuse_or_mutate_an_attempt() {
        let store = Store::in_memory().await.unwrap();
        let first_account = Uuid::new_v4();
        let second_account = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        save_test_account(&store, first_account, "first@example.test").await;
        save_test_account(&store, second_account, "second@example.test").await;
        store
            .claim_send_attempt(first_account, attempt_id, "payload-a")
            .await
            .unwrap();

        assert_eq!(
            store
                .claim_send_attempt(first_account, attempt_id, "payload-b")
                .await
                .unwrap(),
            SendAttemptClaim::Mismatch
        );
        assert!(store
            .complete_send_attempt(
                second_account,
                attempt_id,
                "payload-a",
                SendAttemptStatus::Submitted,
            )
            .await
            .is_err());
        assert_eq!(
            store
                .claim_send_attempt(second_account, attempt_id, "payload-a")
                .await
                .unwrap(),
            SendAttemptClaim::Claimed
        );
    }

    #[tokio::test]
    async fn only_a_definitely_unsubmitted_pending_attempt_can_be_released() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        save_test_account(&store, account_id, "release@example.test").await;

        let unsubmitted = Uuid::new_v4();
        store
            .claim_send_attempt(account_id, unsubmitted, "unsubmitted-payload")
            .await
            .unwrap();
        store
            .release_send_attempt(account_id, unsubmitted, "unsubmitted-payload")
            .await
            .unwrap();
        assert_eq!(
            store
                .claim_send_attempt(account_id, unsubmitted, "unsubmitted-payload")
                .await
                .unwrap(),
            SendAttemptClaim::Claimed
        );

        let finalized = Uuid::new_v4();
        store
            .claim_send_attempt(account_id, finalized, "final-payload")
            .await
            .unwrap();
        store
            .complete_send_attempt(
                account_id,
                finalized,
                "final-payload",
                SendAttemptStatus::DeliveryUncertain,
            )
            .await
            .unwrap();
        assert!(store
            .release_send_attempt(account_id, finalized, "final-payload")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn deleting_an_account_cascades_attempts_and_rejects_new_claims() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        save_test_account(&store, account_id, "deleted@example.test").await;
        store
            .claim_send_attempt(account_id, attempt_id, "payload")
            .await
            .unwrap();
        store.delete_account(account_id).await.unwrap();

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM send_attempts WHERE account_id = ?")
                .bind(account_id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        assert!(store
            .claim_send_attempt(account_id, Uuid::new_v4(), "payload")
            .await
            .is_err());
    }
}
