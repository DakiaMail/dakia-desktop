//! Durable, account-scoped notes attached to logical mail conversations.
//!
//! A provider's mailbox UID and Dakia's calculated thread ID are both mutable.
//! This module therefore keeps a permanent UUID for each logical conversation,
//! and continuously associates it with normalized RFC Message-IDs and current
//! calculated thread IDs.  Context never travels to the mail provider.

use crate::{storage::ConversationTarget, AccountId, Store};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ThreadContextKind {
    Notes,
    AgentMemory,
}

impl ThreadContextKind {
    const fn as_db(self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::AgentMemory => "agent_memory",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ThreadContextAction {
    Set,
    Append,
    Clear,
    Restore,
    Merge,
}

impl ThreadContextAction {
    const fn as_db(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Append => "append",
            Self::Clear => "clear",
            Self::Restore => "restore",
            Self::Merge => "merge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContextLocator {
    pub account_id: AccountId,
    pub conversation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContextConversation {
    pub locator: ThreadContextLocator,
    pub requires_review: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContext {
    pub locator: ThreadContextLocator,
    pub kind: ThreadContextKind,
    pub content: String,
    /// Monotonic per-conversation, per-kind revision. Zero means the document
    /// has never been written and is the expected value for the first write.
    pub revision: u64,
    pub updated_at: Option<DateTime<Utc>>,
    pub requires_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContextRevision {
    pub id: Uuid,
    pub locator: ThreadContextLocator,
    pub kind: ThreadContextKind,
    pub revision: u64,
    pub content: String,
    pub action: ThreadContextAction,
    pub created_at: DateTime<Utc>,
    /// Set when a merge carried history from another permanent conversation.
    pub source_conversation_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContextBundle {
    pub conversation: ThreadContextConversation,
    pub notes: ThreadContext,
    pub agent_memory: ThreadContext,
}

#[derive(Debug, FromRow)]
struct ConversationRow {
    id: String,
    account_id: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    requires_review: bool,
}

#[derive(Debug, FromRow)]
struct DocumentRow {
    content: String,
    revision: i64,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct RevisionRow {
    id: String,
    conversation_id: String,
    kind: String,
    revision: i64,
    content: String,
    action: String,
    created_at: DateTime<Utc>,
    source_conversation_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct ContextThreadRow {
    id: String,
    thread_id: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    reference_ids: Option<String>,
}

#[derive(Default)]
struct ContextThreadGroup {
    local_message_ids: HashSet<String>,
    /// Identities owned by messages in this component. Only these are updated
    /// to point at the assigned permanent conversation.
    message_keys: HashSet<String>,
    /// Message IDs in headers may connect a new reply to an old component,
    /// but a reference is not proof that the referenced message belongs to
    /// this component after a split.
    candidate_keys: HashSet<String>,
}

#[derive(Clone)]
struct ThreadContextTargetGuard {
    target: ConversationTarget,
}

fn context_mailbox_family(mailbox: &str) -> &str {
    mailbox
        .split_once("::")
        .map_or(mailbox, |(family, _)| family)
}

fn context_target_allows_mailbox(mailbox: &str, target: &ConversationTarget) -> bool {
    match target.mailbox.as_deref() {
        Some(expected) if matches!(context_mailbox_family(expected), "Spam" | "Trash") => {
            context_mailbox_family(mailbox) == context_mailbox_family(expected)
        }
        Some(_) | None => !matches!(context_mailbox_family(mailbox), "Spam" | "Trash"),
    }
}

async fn current_thread_for_target_guard(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    target: &ConversationTarget,
) -> Result<Option<String>> {
    let account_id = target.account_id.to_string();
    if let Some(local_message_id) = target
        .local_message_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        let row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT account_id, mailbox, thread_id, message_id FROM messages WHERE id = ?",
        )
        .bind(local_message_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((row_account_id, mailbox, thread_id, message_id)) = row {
            // Match the resolver's fail-closed branch when a local row exists
            // for another account or mailbox family. A missing/stale local ID
            // intentionally falls through to RFC or thread evidence below.
            if row_account_id != account_id || !context_target_allows_mailbox(&mailbox, target) {
                return Ok(None);
            }
            let rfc_matches = target
                .rfc_message_id
                .as_deref()
                .and_then(super::storage::normalize_message_id)
                .is_none_or(|expected| {
                    message_id
                        .as_deref()
                        .and_then(super::storage::normalize_message_id)
                        .as_deref()
                        == Some(expected.as_str())
                });
            let thread_matches = target
                .thread_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .is_none_or(|expected| expected == thread_id);
            if rfc_matches && thread_matches {
                return Ok(Some(thread_id));
            }
        }
    }
    if let Some(rfc_message_id) = target
        .rfc_message_id
        .as_deref()
        .and_then(super::storage::normalize_message_id)
    {
        let threads: HashSet<String> = sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT thread_id, mailbox, message_id FROM messages WHERE account_id = ? AND LOWER(TRIM(message_id)) = ?",
        )
        .bind(&account_id)
        .bind(format!("<{rfc_message_id}>"))
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .filter(|(_, mailbox, message_id)| {
            context_target_allows_mailbox(mailbox, target)
                && message_id
                    .as_deref()
                    .and_then(super::storage::normalize_message_id)
                    .as_deref()
                    == Some(rfc_message_id.as_str())
        })
        .map(|(thread_id, _, _)| thread_id)
        .collect();
        match threads.len() {
            0 => {}
            1 => {
                return Ok(Some(
                    threads
                        .into_iter()
                        .next()
                        .expect("one guarded RFC target has one thread"),
                ));
            }
            _ => return Ok(None),
        }
    }
    let Some(thread_id) = target
        .thread_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    else {
        return Ok(None);
    };
    let mailboxes: Vec<String> =
        sqlx::query_scalar("SELECT mailbox FROM messages WHERE account_id = ? AND thread_id = ?")
            .bind(&account_id)
            .bind(thread_id)
            .fetch_all(&mut **tx)
            .await?;
    Ok(mailboxes
        .iter()
        .any(|mailbox| context_target_allows_mailbox(mailbox, target))
        .then(|| thread_id.to_owned()))
}

fn parse_kind(value: &str) -> Result<ThreadContextKind> {
    match value {
        "notes" => Ok(ThreadContextKind::Notes),
        "agent_memory" => Ok(ThreadContextKind::AgentMemory),
        _ => bail!("stored thread context kind is invalid"),
    }
}

fn parse_action(value: &str) -> Result<ThreadContextAction> {
    match value {
        "set" => Ok(ThreadContextAction::Set),
        "append" => Ok(ThreadContextAction::Append),
        "clear" => Ok(ThreadContextAction::Clear),
        "restore" => Ok(ThreadContextAction::Restore),
        "merge" => Ok(ThreadContextAction::Merge),
        _ => bail!("stored thread context action is invalid"),
    }
}

fn parse_locator(account_id: &str, conversation_id: &str) -> Result<ThreadContextLocator> {
    Ok(ThreadContextLocator {
        account_id: AccountId::parse_str(account_id)
            .context("stored context account ID is invalid")?,
        conversation_id: Uuid::parse_str(conversation_id)
            .context("stored context conversation ID is invalid")?,
    })
}

impl Store {
    async fn thread_context_locator_for_target_mutation(
        &self,
        target: &ConversationTarget,
    ) -> Result<(ThreadContextLocator, ThreadContextTargetGuard)> {
        let mail = self
            .conversation_for_target(target)
            .await?
            .ok_or_else(|| anyhow!("thread context target was not found"))?;
        self.ensure_thread_context_target_ready(&mail.account_id, &mail.thread_id)
            .await?;
        let conversation_id = self
            .context_conversation_id_for_thread(&mail.account_id, &mail.thread_id)
            .await?
            .ok_or_else(|| anyhow!("thread context target is not ready; refresh and retry"))?;
        Ok((
            ThreadContextLocator {
                account_id: AccountId::parse_str(&mail.account_id)
                    .context("mail conversation account ID is invalid")?,
                conversation_id,
            },
            ThreadContextTargetGuard {
                target: target.clone(),
            },
        ))
    }

    /// Resolve a mutable mailbox/thread locator to Dakia's permanent
    /// account-scoped conversation UUID. Missing mail deliberately resolves to
    /// None: a stale deep link must never create a context record by itself.
    pub async fn thread_context_conversation_for_target(
        &self,
        target: &ConversationTarget,
    ) -> Result<Option<ThreadContextConversation>> {
        // Reuse the reader's fail-closed target validation. In particular a
        // local ID may not override an incompatible RFC ID, thread ID, or
        // mailbox scope supplied by a stale notification.
        let Some(mail) = self.conversation_for_target(target).await? else {
            return Ok(None);
        };
        self.ensure_thread_context_target_ready(&mail.account_id, &mail.thread_id)
            .await?;
        let conversation_id = self
            .context_conversation_id_for_thread(&mail.account_id, &mail.thread_id)
            .await?;
        match conversation_id {
            Some(conversation_id) => self
                .thread_context_conversation(&ThreadContextLocator {
                    account_id: AccountId::parse_str(&mail.account_id)
                        .context("mail conversation account ID is invalid")?,
                    conversation_id,
                })
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub async fn thread_context_bundle_for_target(
        &self,
        target: &ConversationTarget,
    ) -> Result<Option<ThreadContextBundle>> {
        let Some(conversation) = self.thread_context_conversation_for_target(target).await? else {
            return Ok(None);
        };
        self.thread_context_bundle(conversation.locator)
            .await
            .map(Some)
    }

    pub async fn thread_context_bundle(
        &self,
        locator: ThreadContextLocator,
    ) -> Result<ThreadContextBundle> {
        let conversation = self.thread_context_conversation(&locator).await?;
        let notes = self
            .thread_context_get(locator.clone(), ThreadContextKind::Notes)
            .await?;
        let agent_memory = self
            .thread_context_get(locator.clone(), ThreadContextKind::AgentMemory)
            .await?;
        Ok(ThreadContextBundle {
            conversation,
            notes,
            agent_memory,
        })
    }

    pub async fn thread_context_get(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
    ) -> Result<ThreadContext> {
        let conversation = self.thread_context_conversation(&locator).await?;
        let row: Option<DocumentRow> = sqlx::query_as(
            "SELECT content, revision, updated_at FROM thread_context_documents WHERE conversation_id = ? AND kind = ?",
        )
        .bind(locator.conversation_id.to_string())
        .bind(kind.as_db())
        .fetch_optional(&self.pool)
        .await?;
        let (content, revision, updated_at) = match row {
            Some(row) => (
                row.content,
                u64::try_from(row.revision).context("stored context revision is invalid")?,
                Some(row.updated_at),
            ),
            None => (String::new(), 0, None),
        };
        Ok(ThreadContext {
            locator,
            kind,
            content,
            revision,
            updated_at,
            requires_review: conversation.requires_review,
        })
    }

    pub async fn thread_context_set(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
        content: impl Into<String>,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        self.write_thread_context(
            locator,
            kind,
            content.into(),
            expected_revision,
            ThreadContextAction::Set,
            None,
        )
        .await
    }

    /// Writes only if the target still resolves to the same permanent
    /// conversation in the write transaction. Use this for mailbox-driven UI,
    /// CLI and agent edits; locator-only writes remain available for explicit
    /// advanced clients.
    pub async fn thread_context_set_for_target(
        &self,
        target: &ConversationTarget,
        kind: ThreadContextKind,
        content: impl Into<String>,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let (locator, guard) = self
            .thread_context_locator_for_target_mutation(target)
            .await?;
        self.write_thread_context(
            locator,
            kind,
            content.into(),
            expected_revision,
            ThreadContextAction::Set,
            Some(&guard),
        )
        .await
    }

    pub async fn thread_context_append(
        &self,
        locator: ThreadContextLocator,
        text: impl AsRef<str>,
        kind: ThreadContextKind,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let current = self.thread_context_get(locator.clone(), kind).await?;
        if current.revision != expected_revision {
            return Err(context_conflict(expected_revision, current.revision));
        }
        let content = if current.content.is_empty() {
            text.as_ref().to_owned()
        } else if text.as_ref().is_empty() {
            current.content
        } else {
            format!("{}\n{}", current.content, text.as_ref())
        };
        self.write_thread_context(
            locator,
            kind,
            content,
            expected_revision,
            ThreadContextAction::Append,
            None,
        )
        .await
    }

    pub async fn thread_context_append_for_target(
        &self,
        target: &ConversationTarget,
        text: impl AsRef<str>,
        kind: ThreadContextKind,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let (locator, guard) = self
            .thread_context_locator_for_target_mutation(target)
            .await?;
        let current = self.thread_context_get(locator.clone(), kind).await?;
        if current.revision != expected_revision {
            return Err(context_conflict(expected_revision, current.revision));
        }
        let content = if current.content.is_empty() {
            text.as_ref().to_owned()
        } else if text.as_ref().is_empty() {
            current.content
        } else {
            format!("{}\n{}", current.content, text.as_ref())
        };
        self.write_thread_context(
            locator,
            kind,
            content,
            expected_revision,
            ThreadContextAction::Append,
            Some(&guard),
        )
        .await
    }

    pub async fn thread_context_clear(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        self.write_thread_context(
            locator,
            kind,
            String::new(),
            expected_revision,
            ThreadContextAction::Clear,
            None,
        )
        .await
    }

    pub async fn thread_context_clear_for_target(
        &self,
        target: &ConversationTarget,
        kind: ThreadContextKind,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let (locator, guard) = self
            .thread_context_locator_for_target_mutation(target)
            .await?;
        self.write_thread_context(
            locator,
            kind,
            String::new(),
            expected_revision,
            ThreadContextAction::Clear,
            Some(&guard),
        )
        .await
    }

    pub async fn thread_context_history(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
    ) -> Result<Vec<ThreadContextRevision>> {
        self.thread_context_conversation(&locator).await?;
        let rows: Vec<RevisionRow> = sqlx::query_as(
            "SELECT id, conversation_id, kind, revision, content, action, created_at, source_conversation_id FROM thread_context_revisions WHERE conversation_id = ? AND kind = ? ORDER BY created_at DESC, id DESC",
        )
        .bind(locator.conversation_id.to_string())
        .bind(kind.as_db())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ThreadContextRevision {
                    id: Uuid::parse_str(&row.id)
                        .context("stored context revision ID is invalid")?,
                    locator: parse_locator(&locator.account_id.to_string(), &row.conversation_id)?,
                    kind: parse_kind(&row.kind)?,
                    revision: u64::try_from(row.revision)
                        .context("stored context revision is invalid")?,
                    content: row.content,
                    action: parse_action(&row.action)?,
                    created_at: row.created_at,
                    source_conversation_id: row
                        .source_conversation_id
                        .map(|value| {
                            Uuid::parse_str(&value)
                                .context("stored source conversation ID is invalid")
                        })
                        .transpose()?,
                })
            })
            .collect()
    }

    pub async fn thread_context_restore(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
        revision_id: Uuid,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM thread_context_revisions WHERE id = ? AND conversation_id = ? AND kind = ?",
        )
        .bind(revision_id.to_string())
        .bind(locator.conversation_id.to_string())
        .bind(kind.as_db())
        .fetch_optional(&self.pool)
        .await?;
        let content = content.ok_or_else(|| anyhow!("thread context revision was not found"))?;
        self.write_thread_context(
            locator,
            kind,
            content,
            expected_revision,
            ThreadContextAction::Restore,
            None,
        )
        .await
    }

    pub async fn thread_context_restore_for_target(
        &self,
        target: &ConversationTarget,
        kind: ThreadContextKind,
        revision_id: Uuid,
        expected_revision: u64,
    ) -> Result<ThreadContext> {
        let (locator, guard) = self
            .thread_context_locator_for_target_mutation(target)
            .await?;
        let content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM thread_context_revisions WHERE id = ? AND conversation_id = ? AND kind = ?",
        )
        .bind(revision_id.to_string())
        .bind(locator.conversation_id.to_string())
        .bind(kind.as_db())
        .fetch_optional(&self.pool)
        .await?;
        let content = content.ok_or_else(|| anyhow!("thread context revision was not found"))?;
        self.write_thread_context(
            locator,
            kind,
            content,
            expected_revision,
            ThreadContextAction::Restore,
            Some(&guard),
        )
        .await
    }

    async fn write_thread_context(
        &self,
        locator: ThreadContextLocator,
        kind: ThreadContextKind,
        content: String,
        expected_revision: u64,
        action: ThreadContextAction,
        target_guard: Option<&ThreadContextTargetGuard>,
    ) -> Result<ThreadContext> {
        // Acquire the SQLite writer reservation before reading the revision.
        // A deferred WAL transaction can otherwise read the same revision in
        // two processes, then fail as SQLITE_BUSY_SNAPSHOT rather than return
        // the documented revision conflict to the losing CLI/desktop editor.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let account_id = locator.account_id.to_string();
        let conversation_id = locator.conversation_id.to_string();
        if let Some(target_guard) = target_guard {
            let Some(thread_id) =
                current_thread_for_target_guard(&mut tx, &target_guard.target).await?
            else {
                return Err(anyhow!("thread context target changed; refresh and retry"));
            };
            let current_target: Option<String> = sqlx::query_scalar(
                "SELECT conversation_id FROM thread_context_thread_links WHERE account_id = ? AND thread_id = ?",
            )
            .bind(target_guard.target.account_id.to_string())
            .bind(thread_id)
            .fetch_optional(&mut *tx)
            .await?;
            if current_target.as_deref() != Some(conversation_id.as_str()) {
                return Err(anyhow!("thread context target changed; refresh and retry"));
            }
        }
        let conversation: Option<ConversationRow> = sqlx::query_as(
            "SELECT id, account_id, created_at, updated_at, requires_review FROM thread_context_conversations WHERE id = ? AND account_id = ?",
        )
        .bind(&conversation_id)
        .bind(&account_id)
        .fetch_optional(&mut *tx)
        .await?;
        let conversation =
            conversation.ok_or_else(|| anyhow!("thread context conversation was not found"))?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM thread_context_documents WHERE conversation_id = ? AND kind = ?",
        )
        .bind(&conversation_id)
        .bind(kind.as_db())
        .fetch_optional(&mut *tx)
        .await?;
        let current = current.unwrap_or(0);
        let expected =
            i64::try_from(expected_revision).context("expected context revision is too large")?;
        if current != expected {
            return Err(context_conflict(
                expected_revision,
                u64::try_from(current).unwrap_or(0),
            ));
        }
        let next = current
            .checked_add(1)
            .ok_or_else(|| anyhow!("thread context revision overflow"))?;
        let now = Utc::now();
        sqlx::query("INSERT INTO thread_context_documents(conversation_id, kind, content, revision, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(conversation_id, kind) DO UPDATE SET content=excluded.content, revision=excluded.revision, updated_at=excluded.updated_at")
            .bind(&conversation_id).bind(kind.as_db()).bind(&content).bind(next).bind(now)
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO thread_context_revisions(id, conversation_id, kind, revision, content, action, created_at, source_conversation_id) VALUES (?, ?, ?, ?, ?, ?, ?, NULL)")
            .bind(Uuid::new_v4().to_string()).bind(&conversation_id).bind(kind.as_db()).bind(next).bind(&content).bind(action.as_db()).bind(now)
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE thread_context_conversations SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(&conversation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ThreadContext {
            locator,
            kind,
            content,
            revision: u64::try_from(next)?,
            updated_at: Some(now),
            requires_review: conversation.requires_review,
        })
    }

    async fn thread_context_conversation(
        &self,
        locator: &ThreadContextLocator,
    ) -> Result<ThreadContextConversation> {
        let row: Option<ConversationRow> = sqlx::query_as(
            "SELECT id, account_id, created_at, updated_at, requires_review FROM thread_context_conversations WHERE id = ? AND account_id = ?",
        )
        .bind(locator.conversation_id.to_string()).bind(locator.account_id.to_string())
        .fetch_optional(&self.pool).await?;
        conversation_from_row(
            row.ok_or_else(|| anyhow!("thread context conversation was not found"))?,
        )
    }

    async fn context_conversation_id_for_thread(
        &self,
        account_id: &str,
        thread_id: &str,
    ) -> Result<Option<Uuid>> {
        let id: Option<String> = sqlx::query_scalar("SELECT conversation_id FROM thread_context_thread_links WHERE account_id = ? AND thread_id = ?")
            .bind(account_id).bind(thread_id).fetch_optional(&self.pool).await?;
        id.map(|id| Uuid::parse_str(&id).context("stored context conversation ID is invalid"))
            .transpose()
    }

    async fn context_conversation_ids_for_message_keys(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        account_id: &str,
        keys: &HashSet<String>,
    ) -> Result<HashMap<String, Uuid>> {
        let mut found = HashMap::new();
        for keys in keys.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; keys.len()].join(", ");
            let sql = format!(
                "SELECT rfc_message_id, conversation_id FROM thread_context_message_keys WHERE account_id = ? AND rfc_message_id IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(account_id);
            for key in keys {
                query = query.bind(*key);
            }
            for (key, conversation_id) in query.fetch_all(&mut **tx).await? {
                found.insert(
                    key,
                    Uuid::parse_str(&conversation_id)
                        .context("stored context conversation ID is invalid")?,
                );
            }
        }
        Ok(found)
    }

    async fn context_conversation_ids_for_local_messages(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        account_id: &str,
        local_message_ids: &HashSet<String>,
    ) -> Result<HashMap<String, Uuid>> {
        let mut found = HashMap::new();
        for ids in local_message_ids.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!(
                "SELECT local_message_id, conversation_id FROM thread_context_local_message_links WHERE account_id = ? AND local_message_id IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(account_id);
            for id in ids {
                query = query.bind(*id);
            }
            for (local_message_id, conversation_id) in query.fetch_all(&mut **tx).await? {
                found.insert(
                    local_message_id,
                    Uuid::parse_str(&conversation_id)
                        .context("stored context conversation ID is invalid")?,
                );
            }
        }
        Ok(found)
    }

    /// Finds every current component that still owns one of the supplied
    /// permanent conversations. This is deliberately scoped by conversation
    /// IDs rather than the selected thread's keys: after a rethread, a split
    /// sibling may be outside the component currently opened by the user.
    /// The query therefore remains bounded to context already relevant to
    /// this repair, instead of scanning an account's whole mailbox.
    async fn current_threads_for_context_conversations(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        account_id: &str,
        conversation_ids: &HashSet<Uuid>,
    ) -> Result<HashMap<Uuid, HashSet<String>>> {
        let mut found: HashMap<Uuid, HashSet<String>> = HashMap::new();
        for ids in conversation_ids.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!(
                "SELECT keys.conversation_id, messages.thread_id \
                 FROM thread_context_message_keys AS keys \
                 JOIN messages INDEXED BY messages_account_normalized_message_id ON messages.account_id = keys.account_id \
                    AND messages.message_id IS NOT NULL \
                    AND LOWER(TRIM(messages.message_id)) = '<' || keys.rfc_message_id || '>' \
                 WHERE keys.account_id = ? AND keys.conversation_id IN ({placeholders}) \
                 UNION ALL \
                 SELECT links.conversation_id, messages.thread_id \
                 FROM thread_context_local_message_links AS links \
                 JOIN messages ON messages.account_id = links.account_id \
                    AND messages.id = links.local_message_id \
                 WHERE links.account_id = ? AND links.conversation_id IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(account_id);
            for id in ids {
                query = query.bind(id.to_string());
            }
            query = query.bind(account_id);
            for id in ids {
                query = query.bind(id.to_string());
            }
            for (conversation_id, thread_id) in query.fetch_all(&mut **tx).await? {
                found
                    .entry(
                        Uuid::parse_str(&conversation_id)
                            .context("stored context conversation ID is invalid")?,
                    )
                    .or_default()
                    .insert(thread_id);
            }
        }
        Ok(found)
    }

    async fn context_conversation_created_at(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        account_id: &str,
        conversation_ids: &HashSet<Uuid>,
    ) -> Result<HashMap<Uuid, DateTime<Utc>>> {
        let mut found = HashMap::new();
        for ids in conversation_ids.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!(
                "SELECT id, created_at FROM thread_context_conversations WHERE account_id = ? AND id IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, (String, DateTime<Utc>)>(&sql).bind(account_id);
            for id in ids {
                query = query.bind(id.to_string());
            }
            for (id, created_at) in query.fetch_all(&mut **tx).await? {
                found.insert(
                    Uuid::parse_str(&id).context("stored context conversation ID is invalid")?,
                    created_at,
                );
            }
        }
        Ok(found)
    }

    async fn current_threads_for_actual_message_keys(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        account_id: &str,
        keys: &HashSet<String>,
    ) -> Result<HashMap<String, HashSet<String>>> {
        let mut found = HashMap::new();
        for keys in keys.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; keys.len()].join(", ");
            let sql = format!(
                "SELECT LOWER(TRIM(message_id)), thread_id FROM messages INDEXED BY messages_account_normalized_message_id WHERE account_id = ? AND message_id IS NOT NULL AND LOWER(TRIM(message_id)) IN ({placeholders})"
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(account_id);
            for key in keys {
                query = query.bind(format!("<{key}>"));
            }
            for (message_id, thread_id) in query.fetch_all(&mut **tx).await? {
                if let Some(key) = super::storage::normalize_message_id(&message_id) {
                    found
                        .entry(key)
                        .or_insert_with(HashSet::new)
                        .insert(thread_id);
                }
            }
        }
        Ok(found)
    }

    /// Repairs only the current component when a rethread invalidated its
    /// link. An absent link is also how a large legacy profile is bootstrapped
    /// lazily, so opening one message never scans a whole mailbox.
    pub(crate) async fn ensure_thread_context_target_ready(
        &self,
        account_id: &str,
        thread_id: &str,
    ) -> Result<()> {
        if self
            .context_conversation_id_for_thread(account_id, thread_id)
            .await?
            .is_none()
        {
            self.sync_thread_context_conversations_for_threads(account_id, Some(&[thread_id]))
                .await?;
        }
        Ok(())
    }

    /// Reconciles every currently stored component for callers that explicitly
    /// request account-wide repair. Interactive target resolution uses the
    /// bounded selected-thread variant above.
    pub(crate) async fn sync_thread_context_conversations(&self, account_id: &str) -> Result<()> {
        self.sync_thread_context_conversations_for_threads(account_id, None)
            .await
    }

    /// Called after threading or by the lazy resolver. It binds permanent
    /// conversation IDs to the freshly calculated thread components, merging
    /// histories when two formerly separate threads join. If an old permanent
    /// conversation now spans multiple components, its context is retained on
    /// one deterministic component and every affected component is marked for
    /// review instead of silently copying notes into unrelated mail.
    async fn sync_thread_context_conversations_for_threads(
        &self,
        account_id: &str,
        threads: Option<&[&str]>,
    ) -> Result<()> {
        // Hold the writer reservation across selection and mutation. A lazy
        // resolver can run in the desktop and CLI at the same time; allowing
        // either one to choose/merge before it observes the other's link can
        // strand context under an unlinked UUID.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some([thread_id]) = threads {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT conversation_id FROM thread_context_thread_links WHERE account_id = ? AND thread_id = ?",
            )
            .bind(account_id)
            .bind(*thread_id)
            .fetch_optional(&mut *tx)
            .await?;
            if existing.is_some() {
                tx.commit().await?;
                return Ok(());
            }
        }
        let rows: Vec<ContextThreadRow> = match threads {
            Some([thread_id]) => sqlx::query_as(
                "SELECT id, thread_id, message_id, in_reply_to, reference_ids FROM messages WHERE account_id = ? AND thread_id = ? ORDER BY received_at, id",
            )
            .bind(account_id)
            .bind(*thread_id)
            .fetch_all(&mut *tx)
            .await?,
            Some(_) => bail!("thread context synchronization requires one selected thread"),
            None => sqlx::query_as(
                "SELECT id, thread_id, message_id, in_reply_to, reference_ids FROM messages WHERE account_id = ? ORDER BY received_at, id",
            )
            .bind(account_id)
            .fetch_all(&mut *tx)
            .await?,
        };
        if rows.is_empty() {
            tx.commit().await?;
            return Ok(());
        }
        let mut groups: HashMap<String, ContextThreadGroup> = HashMap::new();
        for row in rows {
            let group = groups.entry(row.thread_id).or_default();
            group.local_message_ids.insert(row.id);
            if let Some(message_id) = row.message_id {
                let message_keys = super::storage::parse_message_ids(&message_id);
                group.message_keys.extend(message_keys.iter().cloned());
                group.candidate_keys.extend(message_keys);
            }
            for value in [row.in_reply_to, row.reference_ids].into_iter().flatten() {
                group
                    .candidate_keys
                    .extend(super::storage::parse_message_ids(&value));
            }
        }
        let candidate_keys: HashSet<String> = groups
            .values()
            .flat_map(|group| group.candidate_keys.iter().cloned())
            .collect();
        let local_message_ids: HashSet<String> = groups
            .values()
            .flat_map(|group| group.local_message_ids.iter().cloned())
            .collect();
        let actual_message_keys: HashSet<String> = groups
            .values()
            .flat_map(|group| group.message_keys.iter().cloned())
            .collect();
        let actual_key_owners = self
            .current_threads_for_actual_message_keys(&mut tx, account_id, &actual_message_keys)
            .await?;
        let duplicate_actual_keys: HashSet<String> = actual_key_owners
            .iter()
            .filter_map(|(key, threads)| (threads.len() > 1).then_some(key.clone()))
            .collect();
        let duplicate_actual_threads: HashSet<String> = actual_key_owners
            .into_iter()
            .filter(|(_, threads)| threads.len() > 1)
            .flat_map(|(_, threads)| threads)
            .collect();
        let message_contexts = self
            .context_conversation_ids_for_message_keys(&mut tx, account_id, &candidate_keys)
            .await?;
        let local_contexts = self
            .context_conversation_ids_for_local_messages(&mut tx, account_id, &local_message_ids)
            .await?;
        let mut existing_links: HashMap<String, Uuid> = HashMap::new();
        let mut candidate_groups: HashMap<String, HashSet<Uuid>> = HashMap::new();
        let mut owned_candidate_groups: HashMap<String, HashSet<Uuid>> = HashMap::new();
        for (thread_id, group) in &groups {
            let mut candidates = HashSet::new();
            let mut owned_candidates = HashSet::new();
            for key in &group.candidate_keys {
                if !duplicate_actual_keys.contains(key) {
                    if let Some(id) = message_contexts.get(key) {
                        candidates.insert(*id);
                    }
                }
            }
            for key in &group.message_keys {
                if !duplicate_actual_keys.contains(key) {
                    if let Some(id) = message_contexts.get(key) {
                        owned_candidates.insert(*id);
                    }
                }
            }
            for local_message_id in &group.local_message_ids {
                if let Some(id) = local_contexts.get(local_message_id) {
                    candidates.insert(*id);
                    owned_candidates.insert(*id);
                }
            }
            if candidates.is_empty() {
                let id: Option<String> = sqlx::query_scalar("SELECT conversation_id FROM thread_context_thread_links WHERE account_id = ? AND thread_id = ?")
                    .bind(account_id)
                    .bind(thread_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                if let Some(id) = id
                    .map(|id| {
                        Uuid::parse_str(&id).context("stored context conversation ID is invalid")
                    })
                    .transpose()?
                {
                    existing_links.insert(thread_id.clone(), id);
                    candidates.insert(id);
                }
            }
            candidate_groups.insert(thread_id.clone(), candidates);
            owned_candidate_groups.insert(thread_id.clone(), owned_candidates);
        }
        let candidate_conversation_ids: HashSet<Uuid> = candidate_groups
            .values()
            .flat_map(|candidates| candidates.iter().copied())
            .collect();
        let conversation_owners = self
            .current_threads_for_context_conversations(
                &mut tx,
                account_id,
                &candidate_conversation_ids,
            )
            .await?;
        // A prior identity can appear in several newly-calculated components
        // even when it is not selected as every component's winner. Treat
        // that as a split before any merge. Otherwise C{A,B} and D{B} could
        // merge B into A while D still points at the now-empty B document.
        let mut candidate_occurrences: HashMap<Uuid, Vec<String>> = HashMap::new();
        for (thread_id, candidates) in &candidate_groups {
            for conversation_id in candidates {
                candidate_occurrences
                    .entry(*conversation_id)
                    .or_default()
                    .push(thread_id.clone());
            }
        }
        let ambiguous_candidates: HashSet<Uuid> = candidate_occurrences
            .iter()
            .filter_map(|(conversation_id, threads)| {
                (threads.len() > 1).then_some(*conversation_id)
            })
            .collect();
        let mut ambiguous_candidates = ambiguous_candidates;
        for (conversation_id, owner_threads) in &conversation_owners {
            if owner_threads.len() > 1
                || owner_threads
                    .iter()
                    .any(|thread_id| !groups.contains_key(thread_id))
            {
                ambiguous_candidates.insert(*conversation_id);
            }
        }
        let created = self
            .context_conversation_created_at(&mut tx, account_id, &candidate_conversation_ids)
            .await?;

        let mut assignments: HashMap<String, Uuid> = HashMap::new();
        for (thread_id, candidates) in &candidate_groups {
            // Actual message identities outrank References-only evidence.
            // C{A, ref B} must retain A if B still belongs to D.
            let selection = owned_candidate_groups
                .get(thread_id)
                .filter(|owned| !owned.is_empty())
                .unwrap_or(candidates);
            let chosen = selection
                .iter()
                .min_by(|left, right| {
                    created
                        .get(left)
                        .cmp(&created.get(right))
                        .then_with(|| left.cmp(right))
                })
                .copied()
                .or_else(|| existing_links.get(thread_id).copied())
                .unwrap_or_else(Uuid::new_v4);
            assignments.insert(thread_id.clone(), chosen);
        }
        // One old permanent identity in multiple current groups is a split.
        // Preserve its context on the lexicographically first thread and make
        // distinct empty contexts for the other groups, all review-marked.
        let mut owner: HashMap<Uuid, String> = HashMap::new();
        let mut split_threads: HashSet<String> = candidate_occurrences
            .iter()
            .filter(|(conversation_id, _)| ambiguous_candidates.contains(conversation_id))
            .flat_map(|(_, threads)| threads.iter().cloned())
            .collect();
        split_threads.extend(
            duplicate_actual_threads
                .into_iter()
                .filter(|thread_id| groups.contains_key(thread_id)),
        );
        // A candidate omitted here is a prior identity that now belongs to a
        // different component. It must not be merged into the new component:
        // doing so would copy private context across an uncertain split.
        let mut split_candidates = HashSet::new();
        // A selected-thread repair sees only one current group. If the same
        // durable identity still owns messages in a sibling group, preserve it
        // on the deterministic first current thread and give the other group
        // a fresh, review-required identity. Without this, opening T1 and T2
        // after a rethread silently reconnects both sides of a split to X.
        for (thread_id, conversation_id) in assignments.clone() {
            let Some(owner_threads) = conversation_owners.get(&conversation_id) else {
                continue;
            };
            if owner_threads.len() == 1 && owner_threads.contains(&thread_id) {
                continue;
            }
            let canonical_owner = owner_threads
                .iter()
                .min()
                .expect("non-empty owners when length is greater than one");
            split_threads.insert(thread_id.clone());
            if canonical_owner != &thread_id {
                assignments.insert(thread_id.clone(), Uuid::new_v4());
                split_candidates.insert((thread_id, conversation_id));
            }
        }
        for (thread_id, conversation_id) in assignments.clone() {
            if let Some(existing) = owner.get(&conversation_id) {
                if thread_id < *existing {
                    let prior_owner = existing.clone();
                    assignments.insert(prior_owner.clone(), Uuid::new_v4());
                    split_candidates.insert((prior_owner.clone(), conversation_id));
                    owner.insert(conversation_id, thread_id.clone());
                    split_threads.insert(prior_owner);
                    split_threads.insert(thread_id);
                } else {
                    assignments.insert(thread_id.clone(), Uuid::new_v4());
                    split_candidates.insert((thread_id.clone(), conversation_id));
                    split_threads.insert(thread_id);
                }
            } else {
                owner.insert(conversation_id, thread_id);
            }
        }

        let now = Utc::now();
        for conversation_id in assignments.values().copied().collect::<HashSet<_>>() {
            sqlx::query("INSERT OR IGNORE INTO thread_context_conversations(id, account_id, created_at, updated_at, requires_review) VALUES (?, ?, ?, ?, 0)")
                .bind(conversation_id.to_string()).bind(account_id).bind(now).bind(now).execute(&mut *tx).await?;
        }
        for (thread_id, candidates) in &candidate_groups {
            let winner = assignments[thread_id];
            let has_ambiguous_candidate = candidates
                .iter()
                .any(|id| ambiguous_candidates.contains(id));
            for loser in candidates.iter().copied().filter(|id| {
                *id != winner
                    && !has_ambiguous_candidate
                    && !ambiguous_candidates.contains(id)
                    && !split_candidates.contains(&(thread_id.clone(), *id))
            }) {
                merge_thread_context_conversations(&mut tx, winner, loser, now).await?;
            }
        }
        match threads {
            // A selected-thread repair only runs after observing no link. Do
            // not delete here: another Store may have just created the same
            // link. The insert below then becomes a get-or-create operation
            // and both CLI and desktop resolve the durable winner.
            Some([_]) => {}
            Some(_) => unreachable!("the selected-thread query validated one thread"),
            None => {
                sqlx::query("DELETE FROM thread_context_thread_links WHERE account_id = ?")
                    .bind(account_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let mut persisted_assignments = assignments.clone();
        for (thread_id, conversation_id) in &assignments {
            let query = match threads {
                Some([_]) => "INSERT OR IGNORE INTO thread_context_thread_links(account_id, thread_id, conversation_id) VALUES (?, ?, ?)",
                None => "INSERT INTO thread_context_thread_links(account_id, thread_id, conversation_id) VALUES (?, ?, ?)",
                Some(_) => unreachable!("the selected-thread query validated one thread"),
            };
            sqlx::query(query)
                .bind(account_id)
                .bind(thread_id)
                .bind(conversation_id.to_string())
                .execute(&mut *tx)
                .await?;
            if matches!(threads, Some([_])) {
                let persisted: String = sqlx::query_scalar("SELECT conversation_id FROM thread_context_thread_links WHERE account_id = ? AND thread_id = ?")
                    .bind(account_id).bind(thread_id).fetch_one(&mut *tx).await?;
                persisted_assignments.insert(
                    thread_id.clone(),
                    Uuid::parse_str(&persisted)
                        .context("stored context conversation ID is invalid")?,
                );
            }
        }
        // This key was previously allowed to identify one component but is
        // now owned by several. Quarantine it transactionally; otherwise a
        // later disappearance of one duplicate could revive that stale owner
        // and merge its private notes into the surviving component.
        for keys in duplicate_actual_keys.iter().collect::<Vec<_>>().chunks(400) {
            let placeholders = vec!["?"; keys.len()].join(", ");
            let sql = format!(
                "DELETE FROM thread_context_message_keys WHERE account_id = ? AND rfc_message_id IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql).bind(account_id);
            for key in keys {
                query = query.bind(*key);
            }
            query.execute(&mut *tx).await?;
        }
        for (thread_id, conversation_id) in &persisted_assignments {
            for key in groups[thread_id]
                .message_keys
                .iter()
                .filter(|key| !duplicate_actual_keys.contains(*key))
            {
                sqlx::query("INSERT INTO thread_context_message_keys(account_id, rfc_message_id, conversation_id) VALUES (?, ?, ?) ON CONFLICT(account_id, rfc_message_id) DO UPDATE SET conversation_id=excluded.conversation_id")
                    .bind(account_id).bind(key).bind(conversation_id.to_string()).execute(&mut *tx).await?;
            }
            for local_message_id in &groups[thread_id].local_message_ids {
                sqlx::query("INSERT INTO thread_context_local_message_links(account_id, local_message_id, conversation_id) VALUES (?, ?, ?) ON CONFLICT(account_id, local_message_id) DO UPDATE SET conversation_id=excluded.conversation_id")
                    .bind(account_id).bind(local_message_id).bind(conversation_id.to_string()).execute(&mut *tx).await?;
            }
        }
        // If this selected component is the noncanonical half of a split,
        // its original context may remain linked to an unopened sibling.
        // Mark that durable record now so opening the canonical side later
        // cannot make the review requirement depend on navigation order.
        for conversation_id in &ambiguous_candidates {
            sqlx::query("UPDATE thread_context_conversations SET requires_review = 1, updated_at = ? WHERE id = ? AND account_id = ?")
                .bind(now)
                .bind(conversation_id.to_string())
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
        }
        for thread_id in split_threads {
            if let Some(conversation_id) = persisted_assignments.get(&thread_id) {
                sqlx::query("UPDATE thread_context_conversations SET requires_review = 1, updated_at = ? WHERE id = ?")
                    .bind(now).bind(conversation_id.to_string()).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

fn context_conflict(expected: u64, current: u64) -> anyhow::Error {
    anyhow!("thread context revision conflict: expected {expected}, current {current}")
}

fn conversation_from_row(row: ConversationRow) -> Result<ThreadContextConversation> {
    Ok(ThreadContextConversation {
        locator: parse_locator(&row.account_id, &row.id)?,
        requires_review: row.requires_review,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

async fn merge_thread_context_conversations(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    winner: Uuid,
    loser: Uuid,
    now: DateTime<Utc>,
) -> Result<()> {
    let winner = winner.to_string();
    let loser = loser.to_string();
    for kind in [ThreadContextKind::Notes, ThreadContextKind::AgentMemory] {
        let winner_doc: Option<(String, i64)> = sqlx::query_as("SELECT content, revision FROM thread_context_documents WHERE conversation_id = ? AND kind = ?")
            .bind(&winner).bind(kind.as_db()).fetch_optional(&mut **tx).await?;
        let loser_doc: Option<(String, i64)> = sqlx::query_as("SELECT content, revision FROM thread_context_documents WHERE conversation_id = ? AND kind = ?")
            .bind(&loser).bind(kind.as_db()).fetch_optional(&mut **tx).await?;
        let Some((loser_content, _)) = loser_doc else {
            continue;
        };
        let combined = match winner_doc {
            Some((winner_content, _)) if !winner_content.is_empty() && winner_content != loser_content => format!("{winner_content}\n\n[Context retained from merged conversation]\n{loser_content}"),
            Some((winner_content, _)) if !winner_content.is_empty() => winner_content,
            _ => loser_content,
        };
        // Revision numbers remain monotonic after importing a loser whose
        // history may be ahead of the winner's current document.
        let next: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(revision), 0) + 1 FROM (SELECT revision FROM thread_context_documents WHERE conversation_id IN (?, ?) AND kind = ? UNION ALL SELECT revision FROM thread_context_revisions WHERE conversation_id IN (?, ?) AND kind = ?)")
            .bind(&winner).bind(&loser).bind(kind.as_db()).bind(&winner).bind(&loser).bind(kind.as_db()).fetch_one(&mut **tx).await?;
        sqlx::query("INSERT INTO thread_context_documents(conversation_id, kind, content, revision, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(conversation_id, kind) DO UPDATE SET content=excluded.content, revision=excluded.revision, updated_at=excluded.updated_at")
            .bind(&winner).bind(kind.as_db()).bind(&combined).bind(next).bind(now).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO thread_context_revisions(id, conversation_id, kind, revision, content, action, created_at, source_conversation_id) VALUES (?, ?, ?, ?, ?, 'merge', ?, ?)")
            .bind(Uuid::new_v4().to_string()).bind(&winner).bind(kind.as_db()).bind(next).bind(&combined).bind(now).bind(&loser).execute(&mut **tx).await?;
    }
    // Preserve every historical revision under the survivor so history and
    // restore stay available after a thread merge.
    sqlx::query("UPDATE thread_context_revisions SET conversation_id = ?, source_conversation_id = COALESCE(source_conversation_id, ?) WHERE conversation_id = ?")
        .bind(&winner).bind(&loser).bind(&loser).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM thread_context_documents WHERE conversation_id = ?")
        .bind(&loser)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE thread_context_message_keys SET conversation_id = ? WHERE conversation_id = ?",
    )
    .bind(&winner)
    .bind(&loser)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE thread_context_local_message_links SET conversation_id = ? WHERE conversation_id = ?",
    )
    .bind(&winner)
    .bind(&loser)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE thread_context_conversations SET requires_review = 1, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(&winner)
    .execute(&mut **tx)
    .await?;
    // Permanent locators are externally exposed by the CLI. Once a merge
    // has moved all recoverable data to its survivor, reject the loser rather
    // than accepting a write that no current mail can resolve.
    sqlx::query("DELETE FROM thread_context_conversations WHERE id = ?")
        .bind(&loser)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_account(store: &Store, account_id: AccountId) {
        sqlx::query("INSERT INTO accounts(id, email, data, created_at) VALUES (?, ?, '{}', ?)")
            .bind(account_id.to_string())
            .bind(format!("{account_id}@example.test"))
            .bind(Utc::now())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    async fn seed_message(
        store: &Store,
        account_id: AccountId,
        id: &str,
        uid: i64,
        thread_id: &str,
        message_id: &str,
        in_reply_to: Option<&str>,
    ) {
        sqlx::query("INSERT INTO messages(id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, thread_id, subject, from_address, to_addresses, received_at, snippet, body_text) VALUES (?, ?, 'INBOX', ?, ?, ?, NULL, ?, 'Subject', 'sender@example.test', 'you@example.test', ?, '', '')")
            .bind(id).bind(account_id.to_string()).bind(uid).bind(message_id).bind(in_reply_to).bind(thread_id).bind(Utc::now())
            .execute(&store.pool).await.unwrap();
    }

    fn target(account_id: AccountId, local_message_id: &str) -> ConversationTarget {
        ConversationTarget {
            account_id,
            local_message_id: Some(local_message_id.into()),
            rfc_message_id: None,
            thread_id: None,
            mailbox: Some("INBOX".into()),
        }
    }

    #[tokio::test]
    async fn writes_history_restores_and_rejects_stale_revisions() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "original",
            1,
            "thread-a",
            "<root@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let conversation = store
            .thread_context_conversation_for_target(&target(account_id, "original"))
            .await
            .unwrap()
            .unwrap();
        let first = store
            .thread_context_set(
                conversation.locator.clone(),
                ThreadContextKind::Notes,
                "call on Tuesday",
                0,
            )
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        let second = store
            .thread_context_append(
                conversation.locator.clone(),
                "policy number saved",
                ThreadContextKind::Notes,
                1,
            )
            .await
            .unwrap();
        assert_eq!(second.content, "call on Tuesday\npolicy number saved");
        assert!(store
            .thread_context_clear(conversation.locator.clone(), ThreadContextKind::Notes, 1)
            .await
            .unwrap_err()
            .to_string()
            .contains("revision conflict"));
        let history = store
            .thread_context_history(conversation.locator.clone(), ThreadContextKind::Notes)
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        let original = history.iter().find(|entry| entry.revision == 1).unwrap();
        let restored = store
            .thread_context_restore(
                conversation.locator.clone(),
                ThreadContextKind::Notes,
                original.id,
                2,
            )
            .await
            .unwrap();
        assert_eq!(restored.content, "call on Tuesday");
        assert_eq!(restored.revision, 3);
    }

    #[tokio::test]
    async fn context_follows_rfc_message_id_across_uid_namespace_rebuild() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "before-rebuild",
            1,
            "old-thread",
            "<root@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let original = store
            .thread_context_conversation_for_target(&target(account_id, "before-rebuild"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                original.locator.clone(),
                ThreadContextKind::AgentMemory,
                "claim opened",
                0,
            )
            .await
            .unwrap();

        sqlx::query("DELETE FROM messages WHERE id = 'before-rebuild'")
            .execute(&store.pool)
            .await
            .unwrap();
        seed_message(
            &store,
            account_id,
            "after-rebuild",
            42,
            "new-thread",
            "<root@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let rebuilt = store
            .thread_context_bundle_for_target(&target(account_id, "after-rebuild"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rebuilt.conversation.locator, original.locator);
        assert_eq!(rebuilt.agent_memory.content, "claim opened");
    }

    #[tokio::test]
    async fn late_message_id_keeps_context_through_a_rethread() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "late-header",
            1,
            "provisional-thread",
            "<placeholder@example.test>",
            None,
        )
        .await;
        sqlx::query("UPDATE messages SET message_id = NULL WHERE id = 'late-header'")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let original = store
            .thread_context_conversation_for_target(&target(account_id, "late-header"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                original.locator.clone(),
                ThreadContextKind::Notes,
                "provider sent headers later",
                0,
            )
            .await
            .unwrap();

        sqlx::query("UPDATE messages SET message_id = '<late@example.test>', thread_id = 'final-thread' WHERE id = 'late-header'")
            .execute(&store.pool)
            .await
            .unwrap();
        let resolved = store
            .thread_context_bundle_for_target(&target(account_id, "late-header"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.conversation.locator, original.locator);
        assert_eq!(resolved.notes.content, "provider sent headers later");
    }

    #[tokio::test]
    async fn deleting_an_account_removes_owned_context() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "message",
            1,
            "thread",
            "<root@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let context = store
            .thread_context_conversation_for_target(&target(account_id, "message"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                context.locator.clone(),
                ThreadContextKind::Notes,
                "private",
                0,
            )
            .await
            .unwrap();
        store.delete_account(account_id).await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM thread_context_conversations WHERE id = ?")
                .bind(context.locator.conversation_id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn split_does_not_copy_context_into_an_uncertain_component() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "first",
            1,
            "joined-thread",
            "<first@example.test>",
            None,
        )
        .await;
        seed_message(
            &store,
            account_id,
            "second",
            2,
            "joined-thread",
            "<second@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let original = store
            .thread_context_conversation_for_target(&target(account_id, "first"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                original.locator.clone(),
                ThreadContextKind::Notes,
                "only apply where the thread is certain",
                0,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET thread_id = CASE id WHEN 'first' THEN 'thread-one' ELSE 'thread-two' END")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();

        let second = store
            .thread_context_bundle_for_target(&target(account_id, "second"))
            .await
            .unwrap()
            .unwrap();
        let first = store
            .thread_context_bundle_for_target(&target(account_id, "first"))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.conversation.locator, second.conversation.locator);
        assert!(first.conversation.requires_review);
        assert!(second.conversation.requires_review);
        let documents = [first.notes.content, second.notes.content];
        assert_eq!(
            documents
                .iter()
                .filter(|content| content.as_str() == "only apply where the thread is certain")
                .count(),
            1,
            "a split retains context once for review instead of copying it"
        );
    }

    #[tokio::test]
    async fn lazy_split_repair_keeps_a_separate_review_context_for_each_component() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "first",
            1,
            "joined-thread",
            "<first@example.test>",
            None,
        )
        .await;
        seed_message(
            &store,
            account_id,
            "second",
            2,
            "joined-thread",
            "<second@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let original = store
            .thread_context_conversation_for_target(&target(account_id, "first"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                original.locator,
                ThreadContextKind::Notes,
                "only apply where the thread is certain",
                0,
            )
            .await
            .unwrap();

        // This is the production sequence: rethreading atomically removes
        // calculated links, then the reader lazily repairs one selected
        // thread at a time. The second repair must not attach to the first.
        sqlx::query("UPDATE messages SET thread_id = CASE id WHEN 'first' THEN 'thread-one' ELSE 'thread-two' END")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM thread_context_thread_links WHERE account_id = ?")
            .bind(account_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let second = store
            .thread_context_bundle_for_target(&target(account_id, "second"))
            .await
            .unwrap()
            .unwrap();
        let first = store
            .thread_context_bundle_for_target(&target(account_id, "first"))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.conversation.locator, second.conversation.locator);
        assert!(first.conversation.requires_review);
        assert!(second.conversation.requires_review);
        assert_eq!(
            [first.notes.content, second.notes.content]
                .iter()
                .filter(|content| content.as_str() == "only apply where the thread is certain")
                .count(),
            1,
            "selected-thread repair retains an uncertain split once"
        );
    }

    #[tokio::test]
    async fn target_bound_write_rejects_a_split_after_resolution() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "first",
            1,
            "joined-thread",
            "<first@example.test>",
            None,
        )
        .await;
        seed_message(
            &store,
            account_id,
            "second",
            2,
            "joined-thread",
            "<second@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let selected_target = target(account_id, "second");
        let (old_locator, guard) = store
            .thread_context_locator_for_target_mutation(&selected_target)
            .await
            .unwrap();

        // Rebuild splits the selected message into a different permanent
        // context after UI/CLI resolution but before its write transaction.
        sqlx::query("UPDATE messages SET thread_id = CASE id WHEN 'first' THEN 'thread-a' ELSE 'thread-b' END")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let current = store
            .thread_context_conversation_for_target(&selected_target)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(old_locator, current.locator);
        let error = store
            .write_thread_context(
                old_locator,
                ThreadContextKind::Notes,
                "must not follow the other split side".into(),
                0,
                ThreadContextAction::Set,
                Some(&guard),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("thread context target changed; refresh and retry"));
        let after = store
            .thread_context_bundle_for_target(&selected_target)
            .await
            .unwrap()
            .unwrap();
        assert!(after.notes.content.is_empty());
    }

    #[tokio::test]
    async fn target_guard_preserves_resolver_mailbox_and_stale_local_semantics() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "message",
            1,
            "thread",
            "<root@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();

        // A stale local locator may still be accompanied by a valid RFC ID.
        // The target resolver falls back to that ID, and the write guard must
        // preserve the same behavior inside its transaction.
        let moved_local_target = ConversationTarget {
            account_id,
            local_message_id: Some("stale-local-id".into()),
            rfc_message_id: Some("<root@example.test>".into()),
            thread_id: None,
            mailbox: Some("INBOX".into()),
        };
        let written = store
            .thread_context_set_for_target(
                &moved_local_target,
                ThreadContextKind::Notes,
                "valid fallback",
                0,
            )
            .await
            .unwrap();
        assert_eq!(written.content, "valid fallback");

        let inbox_target = target(account_id, "message");
        let (locator, guard) = store
            .thread_context_locator_for_target_mutation(&inbox_target)
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET mailbox = 'Trash' WHERE id = 'message'")
            .execute(&store.pool)
            .await
            .unwrap();
        let error = store
            .write_thread_context(
                locator,
                ThreadContextKind::Notes,
                "must reject moved mailbox".into(),
                1,
                ThreadContextAction::Set,
                Some(&guard),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("thread context target changed; refresh and retry"));
    }

    #[tokio::test]
    async fn merge_keeps_both_contexts_and_their_history() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "alpha",
            1,
            "thread-alpha",
            "<alpha@example.test>",
            None,
        )
        .await;
        seed_message(
            &store,
            account_id,
            "beta",
            2,
            "thread-beta",
            "<beta@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let alpha = store
            .thread_context_conversation_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        let beta = store
            .thread_context_conversation_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                alpha.locator.clone(),
                ThreadContextKind::Notes,
                "alpha context",
                0,
            )
            .await
            .unwrap();
        store
            .thread_context_set(
                beta.locator.clone(),
                ThreadContextKind::Notes,
                "beta context",
                0,
            )
            .await
            .unwrap();
        let beta_latest = store
            .thread_context_append(
                beta.locator.clone(),
                "beta follow-up",
                ThreadContextKind::Notes,
                1,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET thread_id = 'merged-thread'")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let merged = store
            .thread_context_bundle_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        let beta_after = store
            .thread_context_bundle_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.conversation.locator, beta_after.conversation.locator);
        assert!(merged.conversation.requires_review);
        assert!(merged.notes.content.contains("alpha context"));
        assert!(merged.notes.content.contains("beta context"));
        assert!(merged.notes.revision > beta_latest.revision);
        let stale_loser = if merged.conversation.locator == alpha.locator {
            beta.locator
        } else {
            alpha.locator
        };
        let stale_error = store
            .thread_context_set(
                stale_loser,
                ThreadContextKind::Notes,
                "must not be stranded",
                0,
            )
            .await
            .unwrap_err();
        assert!(
            stale_error
                .to_string()
                .contains("thread context conversation was not found"),
            "a merged-away locator must fail so callers re-resolve current mail"
        );
        assert!(
            store
                .thread_context_history(merged.conversation.locator, ThreadContextKind::Notes)
                .await
                .unwrap()
                .len()
                >= 3,
            "both original writes plus the merge record remain restorable"
        );
    }

    #[tokio::test]
    async fn mixed_merge_and_split_never_moves_a_split_context_to_another_thread() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "alpha",
            1,
            "thread-alpha",
            "<alpha@example.test>",
            None,
        )
        .await;
        seed_message(
            &store,
            account_id,
            "beta",
            2,
            "thread-beta",
            "<beta@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let alpha = store
            .thread_context_conversation_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        let beta = store
            .thread_context_conversation_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                alpha.locator.clone(),
                ThreadContextKind::Notes,
                "alpha context",
                0,
            )
            .await
            .unwrap();
        store
            .thread_context_set(
                beta.locator.clone(),
                ThreadContextKind::Notes,
                "beta context",
                0,
            )
            .await
            .unwrap();

        // The alpha component now has evidence for both old conversations,
        // while beta remains a separate current component for B. This used to
        // merge B into A and leave beta linked to an emptied B record.
        sqlx::query("UPDATE messages SET thread_id = 'thread-c', reference_ids = '<beta@example.test>' WHERE id = 'alpha'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET thread_id = 'thread-d' WHERE id = 'beta'")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let alpha_after = store
            .thread_context_bundle_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        let beta_after = store
            .thread_context_bundle_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            alpha_after.conversation.locator,
            beta_after.conversation.locator
        );
        assert!(alpha_after.conversation.requires_review);
        assert!(beta_after.conversation.requires_review);
        assert_eq!(alpha_after.notes.content, "alpha context");
        assert_eq!(beta_after.notes.content, "beta context");
    }

    #[tokio::test]
    async fn duplicate_message_ids_are_quarantined_without_merging_local_context() {
        let store = Store::in_memory().await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&store, account_id).await;
        seed_message(
            &store,
            account_id,
            "alpha",
            1,
            "thread-alpha",
            "<duplicate@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let alpha = store
            .thread_context_conversation_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        store
            .thread_context_set(
                alpha.locator.clone(),
                ThreadContextKind::Notes,
                "alpha private context",
                0,
            )
            .await
            .unwrap();
        seed_message(
            &store,
            account_id,
            "beta",
            2,
            "thread-beta",
            "<duplicate@example.test>",
            None,
        )
        .await;
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let beta = store
            .thread_context_conversation_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(alpha.locator, beta.locator);
        store
            .thread_context_set(
                beta.locator.clone(),
                ThreadContextKind::Notes,
                "beta private context",
                0,
            )
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let alpha_after = store
            .thread_context_bundle_for_target(&target(account_id, "alpha"))
            .await
            .unwrap()
            .unwrap();
        let beta_after = store
            .thread_context_bundle_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alpha_after.conversation.locator, alpha.locator);
        assert_eq!(beta_after.conversation.locator, beta.locator);
        assert_eq!(alpha_after.notes.content, "alpha private context");
        assert_eq!(beta_after.notes.content, "beta private context");
        assert!(alpha_after.conversation.requires_review);
        assert!(beta_after.conversation.requires_review);

        // Once the malformed duplicate disappears, its old RFC association
        // must not reappear and merge alpha's private notes into beta.
        sqlx::query("DELETE FROM messages WHERE id = 'alpha'")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let survivor = store
            .thread_context_bundle_for_target(&target(account_id, "beta"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(survivor.notes.content, "beta private context");
    }

    #[tokio::test]
    async fn concurrent_writers_receive_a_revision_conflict_not_sqlite_busy() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("context.sqlite");
        let first_store = Store::open(&database).await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&first_store, account_id).await;
        seed_message(
            &first_store,
            account_id,
            "message",
            1,
            "thread",
            "<root@example.test>",
            None,
        )
        .await;
        first_store
            .sync_thread_context_conversations(&account_id.to_string())
            .await
            .unwrap();
        let locator = first_store
            .thread_context_conversation_for_target(&target(account_id, "message"))
            .await
            .unwrap()
            .unwrap()
            .locator;
        let second_store = Store::open(&database).await.unwrap();
        let (left, right) = tokio::join!(
            first_store.thread_context_set(locator.clone(), ThreadContextKind::Notes, "first", 0),
            second_store.thread_context_set(locator, ThreadContextKind::Notes, "second", 0)
        );
        assert!(left.is_ok() ^ right.is_ok());
        let conflict = left.err().or_else(|| right.err()).unwrap();
        assert!(
            conflict
                .to_string()
                .contains("thread context revision conflict"),
            "the losing writer must get an actionable CAS conflict: {conflict:#}"
        );
    }

    #[tokio::test]
    async fn concurrent_lazy_resolution_returns_one_permanent_locator() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("context-locator.sqlite");
        let first_store = Store::open(&database).await.unwrap();
        let account_id = Uuid::new_v4();
        seed_account(&first_store, account_id).await;
        seed_message(
            &first_store,
            account_id,
            "message",
            1,
            "thread",
            "<root@example.test>",
            None,
        )
        .await;
        let second_store = Store::open(&database).await.unwrap();
        let first_target = target(account_id, "message");
        let second_target = first_target.clone();
        let (first, second) = tokio::join!(
            first_store.thread_context_conversation_for_target(&first_target),
            second_store.thread_context_conversation_for_target(&second_target),
        );
        let first = first.unwrap().unwrap();
        let second = second.unwrap().unwrap();
        assert_eq!(first.locator, second.locator);
    }
}
