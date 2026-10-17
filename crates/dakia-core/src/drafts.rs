//! Durable local drafts shared by the desktop, CLI, and agent.
//!
//! The provider copy is a best-effort reconciliation target.  The SQLite row
//! remains the source of truth for a user's unfinished work, including when a
//! provider's Drafts folder is unavailable.

use crate::{
    operations::{AccountOperationKind, AccountOperationLeaseState},
    Account, AccountId, ComposeMessage, MailService, Store,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftSyncStatus {
    /// The local revision has not yet been appended to the provider Drafts
    /// folder, or a later edit superseded the provider copy.
    Pending,
    /// The current revision was appended to the provider Drafts folder.
    Synced,
    /// Provider Drafts reconciliation failed. The local draft is safe and
    /// may be reconciled again.
    Failed,
    /// A send was attempted and the result is unknown. This state is a hard
    /// retry barrier because SMTP can accept a message before a disconnect.
    SendUncertain,
    /// SMTP accepted the message and Dakia durably recorded that result.
    Sent,
}

impl DraftSyncStatus {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "synced" => Ok(Self::Synced),
            "failed" => Ok(Self::Failed),
            "send_uncertain" => Ok(Self::SendUncertain),
            "sent" => Ok(Self::Sent),
            _ => bail!("local draft has an invalid status"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalDraft {
    pub id: String,
    pub account_id: AccountId,
    pub revision: i64,
    pub status: DraftSyncStatus,
    pub provider_uid: Option<u32>,
    pub provider_uid_validity: Option<u32>,
    pub provider_mailbox: Option<String>,
    /// True when an older Dakia-created provider copy remains and must be
    /// removed before the lifecycle is fully reconciled.
    pub cleanup_pending: bool,
    pub compose: ComposeMessage,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DraftReconcileResult {
    pub attempted: usize,
    pub synced: usize,
    pub failed: usize,
}

#[derive(Debug, FromRow)]
struct LocalDraftRow {
    id: String,
    account_id: String,
    revision: i64,
    status: String,
    provider_uid: Option<i64>,
    provider_uid_validity: Option<i64>,
    provider_mailbox: Option<String>,
    cleanup_pending: bool,
    compose_json: String,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct ProviderCopyRow {
    uid: i64,
    uid_validity: i64,
    mailbox: String,
}

impl TryFrom<LocalDraftRow> for LocalDraft {
    type Error = anyhow::Error;

    fn try_from(row: LocalDraftRow) -> Result<Self> {
        Ok(Self {
            id: row.id,
            account_id: AccountId::parse_str(&row.account_id)?,
            revision: row.revision,
            status: DraftSyncStatus::parse(&row.status)?,
            provider_uid: row.provider_uid.map(u32::try_from).transpose()?,
            provider_uid_validity: row.provider_uid_validity.map(u32::try_from).transpose()?,
            provider_mailbox: row.provider_mailbox,
            cleanup_pending: row.cleanup_pending,
            compose: serde_json::from_str(&row.compose_json)
                .context("local draft contains invalid compose data")?,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Clone)]
pub struct DraftService {
    store: Store,
    mail: MailService,
}

impl DraftService {
    pub fn new(store: Store, mail: MailService) -> Self {
        Self { store, mail }
    }

    pub async fn create(&self, compose: ComposeMessage) -> Result<LocalDraft> {
        validate_compose(&compose)?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let compose_json = serde_json::to_string(&compose)?;
        sqlx::query("INSERT INTO local_drafts(id, account_id, revision, status, provider_uid, provider_uid_validity, provider_mailbox, compose_json, last_error, created_at, updated_at) VALUES (?, ?, 1, 'pending', NULL, NULL, NULL, ?, NULL, ?, ?)")
            .bind(&id)
            .bind(compose.account_id.to_string())
            .bind(compose_json)
            .bind(now)
            .bind(now)
            .execute(&self.store.pool)
            .await?;
        self.get(&id)
            .await?
            .context("new local draft was not saved")
    }

    pub async fn get(&self, id: &str) -> Result<Option<LocalDraft>> {
        let row = sqlx::query_as::<_, LocalDraftRow>("SELECT d.id, d.account_id, d.revision, d.status, d.provider_uid, d.provider_uid_validity, d.provider_mailbox, EXISTS(SELECT 1 FROM local_draft_provider_copies c WHERE c.draft_id = d.id AND c.cleanup_pending = 1) AS cleanup_pending, d.compose_json, d.last_error, d.created_at, d.updated_at FROM local_drafts d WHERE d.id = ?")
            .bind(id)
            .fetch_optional(&self.store.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub async fn list(&self, account_id: Option<AccountId>) -> Result<Vec<LocalDraft>> {
        let rows = if let Some(account_id) = account_id {
            sqlx::query_as::<_, LocalDraftRow>("SELECT d.id, d.account_id, d.revision, d.status, d.provider_uid, d.provider_uid_validity, d.provider_mailbox, EXISTS(SELECT 1 FROM local_draft_provider_copies c WHERE c.draft_id = d.id AND c.cleanup_pending = 1) AS cleanup_pending, d.compose_json, d.last_error, d.created_at, d.updated_at FROM local_drafts d WHERE d.account_id = ? ORDER BY d.updated_at DESC, d.id DESC")
                .bind(account_id.to_string())
                .fetch_all(&self.store.pool)
                .await?
        } else {
            sqlx::query_as::<_, LocalDraftRow>("SELECT d.id, d.account_id, d.revision, d.status, d.provider_uid, d.provider_uid_validity, d.provider_mailbox, EXISTS(SELECT 1 FROM local_draft_provider_copies c WHERE c.draft_id = d.id AND c.cleanup_pending = 1) AS cleanup_pending, d.compose_json, d.last_error, d.created_at, d.updated_at FROM local_drafts d ORDER BY d.updated_at DESC, d.id DESC")
                .fetch_all(&self.store.pool)
                .await?
        };
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Replaces a draft only when the caller still owns its exact revision.
    /// An optimistic desktop editor and a CLI editor therefore cannot silently
    /// overwrite one another.
    pub async fn update(
        &self,
        id: &str,
        expected_revision: i64,
        compose: ComposeMessage,
    ) -> Result<LocalDraft> {
        validate_compose(&compose)?;
        let current = self.get(id).await?.context("local draft was not found")?;
        if current.account_id != compose.account_id {
            bail!("a local draft cannot be moved between accounts");
        }
        if matches!(
            current.status,
            DraftSyncStatus::SendUncertain | DraftSyncStatus::Sent
        ) {
            bail!("a sent or delivery-uncertain draft cannot be edited");
        }
        let now = Utc::now();
        let mut transaction = self.store.pool.begin().await?;
        sqlx::query("UPDATE local_draft_provider_copies SET cleanup_pending = 1 WHERE draft_id = ? AND cleanup_pending = 0")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("UPDATE local_drafts SET revision = revision + 1, status = 'pending', provider_uid = NULL, provider_uid_validity = NULL, provider_mailbox = NULL, compose_json = ?, last_error = NULL, updated_at = ? WHERE id = ? AND revision = ? AND status NOT IN ('send_uncertain', 'sent')")
            .bind(serde_json::to_string(&compose)?)
            .bind(now)
            .bind(id)
            .bind(expected_revision)
            .execute(&mut *transaction)
            .await?;
        if result.rows_affected() != 1 {
            transaction.rollback().await?;
            bail!("local draft changed in another client; reload it before saving");
        }
        transaction.commit().await?;
        self.get(id)
            .await?
            .context("updated local draft was not found")
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let Some(draft) = self.get(id).await? else {
            return Ok(false);
        };
        let account = self
            .store
            .account(draft.account_id)
            .await?
            .context("draft account no longer exists")?;
        let lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            self.validate_current_enabled_account(&account).await?;
            self.cleanup_all_provider_copies(&account, id, &lease)
                .await?;
            ensure_active(&lease).await?;
            let result = sqlx::query("DELETE FROM local_drafts WHERE id = ?")
                .bind(id)
                .execute(&self.store.pool)
                .await?;
            Ok(result.rows_affected() == 1)
        }
        .await;
        let release = lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(deleted), Ok(())) => Ok(deleted),
        }
    }

    /// Appends the current local revision, records its exact UID namespace,
    /// then removes superseded Dakia-owned copies when possible.
    pub async fn reconcile(&self, account: &Account, id: &str) -> Result<LocalDraft> {
        let lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            self.validate_current_enabled_account(account).await?;
            let draft = self.require_account_draft(account, id).await?;
            if matches!(
                draft.status,
                DraftSyncStatus::SendUncertain | DraftSyncStatus::Sent
            ) {
                return Ok(draft);
            }
            match self
                .mail
                .save_draft_to_provider(account, &draft.compose)
                .await
            {
                Ok(append) => {
                    ensure_active(&lease).await?;
                    self.record_provider_append(id, draft.revision, &append)
                        .await?;
                    self.cleanup_provider_copies(account, id, &lease).await?;
                    self.get(id)
                        .await?
                        .context("reconciled local draft was not found")
                }
                Err(error) => {
                    ensure_active(&lease).await?;
                    self.set_reconcile_failure(id, draft.revision, &error.to_string())
                        .await?;
                    Err(error).context(
                        "local draft remains available but could not be saved to provider Drafts",
                    )
                }
            }
        }
        .await;
        let release = lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(draft), Ok(())) => Ok(draft),
        }
    }

    /// Reconciles a bounded batch of ordinary local edits. Delivery-uncertain
    /// and sent drafts are deliberately excluded: this routine never sends.
    pub async fn reconcile_pending(
        &self,
        account: &Account,
        limit: usize,
    ) -> Result<DraftReconcileResult> {
        // Do not deserialize every draft's compose JSON or attachment payload
        // merely to select a bounded reconciliation batch.
        let limit = i64::try_from(limit.clamp(1, 100))?;
        let candidates = sqlx::query_scalar::<_, String>(
            "SELECT id FROM local_drafts WHERE account_id = ? AND status IN ('pending', 'failed') ORDER BY updated_at ASC, id ASC LIMIT ?",
        )
        .bind(account.id.to_string())
        .bind(limit)
        .fetch_all(&self.store.pool)
        .await?;
        let mut result = DraftReconcileResult::default();
        for id in candidates {
            result.attempted += 1;
            match self.reconcile(account, &id).await {
                Ok(_) => result.synced += 1,
                Err(_) => result.failed += 1,
            }
        }
        Ok(result)
    }

    /// Starts with a durable delivery-uncertain transition before opening SMTP.
    /// Any error after that point is intentionally non-retryable through this
    /// API, because an SMTP server may have accepted the message already.
    pub async fn send(&self, account: &Account, id: &str) -> Result<String> {
        let draft = self.require_account_draft(account, id).await?;
        self.send_revision(account, id, draft.revision).await
    }

    /// Sends exactly the revision a caller reviewed. A later edit cannot race
    /// an approval and be sent under the earlier approval token.
    pub async fn send_revision(
        &self,
        account: &Account,
        id: &str,
        expected_revision: i64,
    ) -> Result<String> {
        let lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            self.validate_current_enabled_account(account).await?;
            let draft = self.require_account_draft(account, id).await?;
            if draft.revision != expected_revision {
                bail!("local draft changed since it was reviewed; review it again before sending");
            }
            if matches!(
                draft.status,
                DraftSyncStatus::SendUncertain | DraftSyncStatus::Sent
            ) {
                bail!("this draft has already been sent or its delivery is uncertain; create a new reviewed draft before sending again");
            }
            let claimed = sqlx::query("UPDATE local_drafts SET status = 'send_uncertain', last_error = NULL, updated_at = ? WHERE id = ? AND revision = ? AND status NOT IN ('send_uncertain', 'sent')")
                .bind(Utc::now())
                .bind(id)
                .bind(expected_revision)
                .execute(&self.store.pool)
                .await?;
            if claimed.rows_affected() != 1 {
                bail!("local draft changed in another client; reload it before sending");
            }
            let response = match self.mail.send(account, &draft.compose).await {
                Ok(response) => response,
                Err(error) => {
                    ensure_active(&lease).await?;
                    let _ = sqlx::query("UPDATE local_drafts SET last_error = ?, updated_at = ? WHERE id = ? AND status = 'send_uncertain'")
                        .bind(error.to_string())
                        .bind(Utc::now())
                        .bind(id)
                        .execute(&self.store.pool)
                        .await;
                    return Err(error).context(
                        "delivery is uncertain; Dakia will not retry this draft automatically",
                    );
                }
            };
            ensure_active(&lease).await?;
            let recorded = sqlx::query("UPDATE local_drafts SET status = 'sent', last_error = NULL, updated_at = ? WHERE id = ? AND status = 'send_uncertain'")
                .bind(Utc::now())
                .bind(id)
                .execute(&self.store.pool)
                .await?;
            if recorded.rows_affected() != 1 {
                return Err(anyhow!("SMTP accepted the message but Dakia could not record it as sent; delivery is uncertain and this draft is blocked from retry"));
            }
            Ok(response)
        }
        .await;
        let release = lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(response), Ok(())) => Ok(response),
        }
    }

    async fn require_account_draft(&self, account: &Account, id: &str) -> Result<LocalDraft> {
        let draft = self.get(id).await?.context("local draft was not found")?;
        if draft.account_id != account.id || draft.compose.account_id != account.id {
            bail!("local draft belongs to a different account");
        }
        Ok(draft)
    }

    async fn validate_current_enabled_account(&self, account: &Account) -> Result<()> {
        let stored = self
            .store
            .account(account.id)
            .await?
            .context("account no longer exists")?;
        if !stored.enabled {
            bail!("account is disabled");
        }
        if serde_json::to_value(&stored)? != serde_json::to_value(account)? {
            bail!("account settings changed since this action was approved; review it again");
        }
        Ok(())
    }

    async fn record_provider_append(
        &self,
        id: &str,
        revision: i64,
        append: &crate::mail::ProviderDraftAppend,
    ) -> Result<()> {
        let mut tx = self.store.pool.begin().await?;
        if let Some(uid) = append.uid {
            sqlx::query("INSERT OR IGNORE INTO local_draft_provider_copies(draft_id, uid, uid_validity, mailbox, revision, cleanup_pending) VALUES (?, ?, ?, ?, ?, 0)")
                .bind(id)
                .bind(i64::from(uid))
                .bind(i64::from(append.uid_validity))
                .bind(&append.mailbox)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }
        let result = sqlx::query("UPDATE local_drafts SET status = 'synced', provider_uid = ?, provider_uid_validity = ?, provider_mailbox = ?, last_error = NULL, updated_at = ? WHERE id = ? AND revision = ? AND status NOT IN ('send_uncertain', 'sent')")
            .bind(append.uid.map(i64::from))
            .bind(append.uid.map(|_| i64::from(append.uid_validity)))
            .bind(append.uid.as_ref().map(|_| &append.mailbox))
            .bind(Utc::now())
            .bind(id)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() != 1 {
            // The append is real even if an edit won this race. Keep it for
            // exact later cleanup instead of losing its identity.
            sqlx::query("UPDATE local_draft_provider_copies SET cleanup_pending = 1 WHERE draft_id = ? AND revision = ?")
                .bind(id)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn set_reconcile_failure(&self, id: &str, revision: i64, error: &str) -> Result<()> {
        sqlx::query("UPDATE local_drafts SET status = 'failed', last_error = ?, updated_at = ? WHERE id = ? AND revision = ? AND status NOT IN ('send_uncertain', 'sent')")
            .bind(error)
            .bind(Utc::now())
            .bind(id)
            .bind(revision)
            .execute(&self.store.pool)
            .await?;
        Ok(())
    }

    async fn cleanup_provider_copies(
        &self,
        account: &Account,
        id: &str,
        lease: &crate::operations::AccountOperationLease,
    ) -> Result<()> {
        let copies = sqlx::query_as::<_, ProviderCopyRow>("SELECT uid, uid_validity, mailbox FROM local_draft_provider_copies WHERE draft_id = ? AND cleanup_pending = 1 ORDER BY revision, uid")
            .bind(id)
            .fetch_all(&self.store.pool)
            .await?;
        for copy in copies {
            let uid = u32::try_from(copy.uid)?;
            let uid_validity = u32::try_from(copy.uid_validity)?;
            if let Err(error) = self
                .mail
                .delete_provider_draft_copy(account, &copy.mailbox, uid, uid_validity)
                .await
            {
                sqlx::query("UPDATE local_drafts SET last_error = ?, updated_at = ? WHERE id = ?")
                    .bind(format!("provider draft cleanup pending: {error}"))
                    .bind(Utc::now())
                    .bind(id)
                    .execute(&self.store.pool)
                    .await?;
                return Err(error).context("provider draft cleanup is pending");
            }
            ensure_active(lease).await?;
            sqlx::query("DELETE FROM local_draft_provider_copies WHERE draft_id = ? AND uid = ? AND uid_validity = ? AND mailbox = ?")
                .bind(id)
                .bind(copy.uid)
                .bind(copy.uid_validity)
                .bind(&copy.mailbox)
                .execute(&self.store.pool)
                .await?;
        }
        Ok(())
    }

    async fn cleanup_all_provider_copies(
        &self,
        account: &Account,
        id: &str,
        lease: &crate::operations::AccountOperationLease,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE local_draft_provider_copies SET cleanup_pending = 1 WHERE draft_id = ?",
        )
        .bind(id)
        .execute(&self.store.pool)
        .await?;
        self.cleanup_provider_copies(account, id, lease).await
    }
}

async fn ensure_active(lease: &crate::operations::AccountOperationLease) -> Result<()> {
    match lease.state().await? {
        AccountOperationLeaseState::Active => Ok(()),
        AccountOperationLeaseState::Lost => bail!("Dakia lost ownership of this account operation"),
        AccountOperationLeaseState::AccountDeleted => bail!("account was removed"),
        AccountOperationLeaseState::AccountMissing => bail!("account does not exist"),
    }
}

fn validate_compose(compose: &ComposeMessage) -> Result<()> {
    if compose.subject.contains(['\r', '\n', '\0']) {
        bail!("draft subject contains invalid control characters");
    }
    if compose.body_text.contains('\0')
        || compose
            .body_html
            .as_deref()
            .is_some_and(|body| body.contains('\0'))
    {
        bail!("draft body contains invalid control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider, AccountDraft};

    fn account() -> Account {
        AccountDraft {
            email: "reader@example.test".into(),
            display_name: "Reader".into(),
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
        .into_account(provider::by_id("fastmail").unwrap())
    }

    fn compose(account_id: AccountId, subject: &str) -> ComposeMessage {
        ComposeMessage {
            account_id,
            to: vec!["recipient@example.test".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            body_text: "hello".into(),
            body_html: None,
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn update_requires_the_current_revision_and_preserves_the_local_draft() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let service = DraftService::new(store.clone(), MailService::new(store));
        let draft = service
            .create(compose(account.id, "Initial"))
            .await
            .unwrap();
        let updated = service
            .update(&draft.id, draft.revision, compose(account.id, "Changed"))
            .await
            .unwrap();
        assert_eq!(updated.revision, 2);
        let error = service
            .update(&draft.id, draft.revision, compose(account.id, "Lost edit"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed in another client"));
        assert_eq!(
            service
                .get(&draft.id)
                .await
                .unwrap()
                .unwrap()
                .compose
                .subject,
            "Changed"
        );
    }

    #[tokio::test]
    async fn sent_and_uncertain_drafts_are_not_editable_or_sendable_again() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let service = DraftService::new(store.clone(), MailService::new(store.clone()));
        let draft = service
            .create(compose(account.id, "Initial"))
            .await
            .unwrap();
        sqlx::query("UPDATE local_drafts SET status = 'send_uncertain' WHERE id = ?")
            .bind(&draft.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(service
            .update(&draft.id, draft.revision, compose(account.id, "Changed"))
            .await
            .is_err());
        assert!(service.send(&account, &draft.id).await.is_err());
    }

    #[tokio::test]
    async fn send_revision_rejects_an_edited_draft_before_opening_smtp() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let service = DraftService::new(store.clone(), MailService::new(store));
        let draft = service
            .create(compose(account.id, "Initial"))
            .await
            .unwrap();
        service
            .update(
                &draft.id,
                draft.revision,
                compose(account.id, "Reviewed edit"),
            )
            .await
            .unwrap();

        let error = service
            .send_revision(&account, &draft.id, draft.revision)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed since it was reviewed"));
    }

    #[tokio::test]
    async fn send_revision_rejects_a_changed_account_before_opening_smtp() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let service = DraftService::new(store.clone(), MailService::new(store.clone()));
        let draft = service
            .create(compose(account.id, "Initial"))
            .await
            .unwrap();
        let mut changed = account.clone();
        changed.display_name = "Changed sender".into();
        store.save_account(&changed).await.unwrap();

        let error = service
            .send_revision(&account, &draft.id, draft.revision)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("account settings changed"));
        assert_eq!(
            service.get(&draft.id).await.unwrap().unwrap().status,
            DraftSyncStatus::Pending
        );
    }

    #[tokio::test]
    async fn same_account_operations_release_the_lease_after_validation_errors() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let service = DraftService::new(store.clone(), MailService::new(store.clone()));

        let delete_draft = service
            .create(compose(account.id, "Delete after validation"))
            .await
            .unwrap();
        let mut disabled = account.clone();
        disabled.enabled = false;
        store.save_account(&disabled).await.unwrap();
        assert!(service.delete(&delete_draft.id).await.is_err());
        store.save_account(&account).await.unwrap();
        assert!(service.delete(&delete_draft.id).await.unwrap());

        let reconcile_draft = service
            .create(compose(account.id, "Reconcile after validation"))
            .await
            .unwrap();
        sqlx::query("UPDATE local_drafts SET status = 'sent' WHERE id = ?")
            .bind(&reconcile_draft.id)
            .execute(&store.pool)
            .await
            .unwrap();
        store.save_account(&disabled).await.unwrap();
        assert!(service
            .reconcile(&account, &reconcile_draft.id)
            .await
            .is_err());
        store.save_account(&account).await.unwrap();
        assert_eq!(
            service
                .reconcile(&account, &reconcile_draft.id)
                .await
                .unwrap()
                .status,
            DraftSyncStatus::Sent
        );

        let send_draft = service
            .create(compose(account.id, "Send after validation"))
            .await
            .unwrap();
        let first = service
            .send_revision(&account, &send_draft.id, 0)
            .await
            .unwrap_err();
        assert!(first.to_string().contains("changed since it was reviewed"));
        let second = service
            .send_revision(&account, &send_draft.id, 0)
            .await
            .unwrap_err();
        assert!(second.to_string().contains("changed since it was reviewed"));
        assert!(!second.to_string().contains("operation is active"));
    }
}
