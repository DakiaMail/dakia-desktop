//! Durable delivery attempts. A failed or interrupted attempt is never retried implicitly.
use crate::Store;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AgentAction {
    pub id: String,
    pub account_id: String,
    pub kind: String,
    pub binding_hash: String,
    pub status: String,
    pub response: Option<String>,
    pub created_at: String,
}

impl Store {
    /// Project bounded summaries in SQLite so attachment data and message bodies
    /// are never loaded merely to choose a draft.
    pub async fn agent_draft_summaries(
        &self,
        account_id: Option<uuid::Uuid>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<serde_json::Value>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "WITH selected AS MATERIALIZED (
                 SELECT id FROM local_drafts WHERE (? IS NULL OR account_id = ?)
                 ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?
             )
             SELECT json_object('id', d.id, 'accountId', account_id, 'revision', revision,
             'status', status, 'subject', substr(json_extract(compose_json, '$.subject'), 1, 2000),
             'to', json((SELECT json_group_array(value) FROM
                 (SELECT value FROM json_each(compose_json, '$.to') LIMIT 20))),
             'attachmentCount', coalesce(json_array_length(compose_json, '$.attachments'), 0),
             'updatedAt', updated_at)
             FROM selected JOIN local_drafts d ON d.id = selected.id
             ORDER BY updated_at DESC, d.id DESC",
        )
        .bind(account_id.map(|id| id.to_string()))
        .bind(account_id.map(|id| id.to_string()))
        .bind(limit.clamp(1, 100))
        .bind(offset.min(100_000))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(Into::into))
            .collect()
    }

    async fn initialize_agent_actions(&self) -> Result<()> {
        sqlx::query("CREATE TABLE IF NOT EXISTS agent_actions (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, kind TEXT NOT NULL, binding_hash TEXT NOT NULL, status TEXT NOT NULL, response TEXT, created_at TEXT NOT NULL)")
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Claim exactly one action before an external side effect. `uncertain` is
    /// deliberate: a process crash after SMTP acceptance must not permit resend.
    pub async fn agent_begin_action(
        &self,
        id: &str,
        account_id: uuid::Uuid,
        kind: &str,
        binding_hash: &str,
    ) -> Result<()> {
        self.initialize_agent_actions().await?;
        if self.account(account_id).await?.is_none() {
            bail!("account not found");
        }
        let result = sqlx::query("INSERT OR IGNORE INTO agent_actions(id, account_id, kind, binding_hash, status, created_at) VALUES (?, ?, ?, ?, 'uncertain', ?)")
            .bind(id).bind(account_id.to_string()).bind(kind).bind(binding_hash)
            .bind(chrono::Utc::now().to_rfc3339()).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            bail!("This action was already attempted. Check the delivery result before sending again.");
        }
        Ok(())
    }

    pub async fn agent_finish_action(
        &self,
        id: &str,
        status: &str,
        response: Option<&str>,
    ) -> Result<()> {
        if !matches!(status, "sent" | "completed" | "uncertain" | "failed") {
            bail!("invalid action status");
        }
        self.initialize_agent_actions().await?;
        let result = sqlx::query("UPDATE agent_actions SET status = ?, response = ? WHERE id = ?")
            .bind(status)
            .bind(response)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            bail!("action not found");
        }
        Ok(())
    }

    pub async fn agent_actions(&self) -> Result<Vec<AgentAction>> {
        self.initialize_agent_actions().await?;
        Ok(sqlx::query_as("SELECT id, account_id, kind, binding_hash, status, response, created_at FROM agent_actions ORDER BY created_at DESC LIMIT 200")
            .fetch_all(&self.pool).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn crashed_or_completed_attempt_cannot_be_reclaimed() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.unwrap();
        let input: crate::AccountDraft = serde_json::from_value(
            serde_json::json!({"email":"ledger@example.test","display_name":"Ledger fixture"}),
        )
        .unwrap();
        let account = input.into_account(crate::provider::by_id("custom").unwrap());
        store.save_account(&account).await.unwrap();
        store
            .agent_begin_action("attempt", account.id, "send_draft", "digest")
            .await
            .unwrap();
        drop(store);
        let store = Store::open(&database).await.unwrap();
        assert!(store
            .agent_begin_action("attempt", account.id, "send_draft", "digest")
            .await
            .is_err());
        assert_eq!(store.agent_actions().await.unwrap()[0].status, "uncertain");
        store
            .agent_finish_action("attempt", "sent", Some("accepted"))
            .await
            .unwrap();
        assert!(store
            .agent_begin_action("attempt", account.id, "send_draft", "digest")
            .await
            .is_err());
    }
}
