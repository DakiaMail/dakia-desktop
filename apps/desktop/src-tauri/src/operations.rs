//! Durable operation-journal helpers for per-message mutations.
//!
//! The helpers here deliberately stage a journal entry before the caller
//! changes local presentation state or contacts a provider.  The operation's
//! target contains the mailbox UIDVALIDITY observed for the message, so a
//! delayed worker cannot replay it against a reused UID after the mailbox is
//! rebuilt.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use dakia_core::mail::mailbox_action_outcome_is_uncertain;
use dakia_core::{
    mailbox_action_destination,
    storage::{MailSummary, OperationJournalEntry, Store},
    Account, MailService, MailboxAction,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::outgoing::ClaimHeartbeat;

pub const READ_MUTATION_KIND: &str = "message_read";
pub const STAR_MUTATION_KIND: &str = "message_star";
pub const MAILBOX_ACTION_KIND: &str = "mailbox_action";

/// Summary returned by a bounded journal drain. `unresolved` entries retain
/// their fence and require a later reconciliation rather than an automatic
/// replay against a target whose provider outcome is not known.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MutationDrainReport {
    pub claimed: usize,
    pub completed: usize,
    pub retried: usize,
    pub rolled_back: usize,
    pub unresolved: usize,
    pub updates: Vec<MutationOperationUpdate>,
}

/// A durable worker transition that callers can surface to the optimistic UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOperationUpdate {
    pub operation_id: String,
    pub account_id: String,
    pub message_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub error: Option<String>,
}

/// The complete local intent needed to replay or restore a message mutation.
///
/// `previous` is captured from the message that the user acted on.  A caller
/// restoring it after a permanent failure must first ensure this operation is
/// still the current mutation for the locator; a newer optimistic operation
/// must win over an older failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mutation", rename_all = "snake_case")]
pub enum MessageMutation {
    Read { value: bool, previous: bool },
    Star { value: bool, previous: bool },
    MailboxAction { action: MailboxAction },
}

/// The provider mailbox name must be captured with the mutation intent. A
/// local catalogue name such as `Archive` is not necessarily valid for IMAP,
/// and resolving it later could target a different mailbox after settings or
/// a replacement generation change.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalMessageMutation {
    #[serde(flatten)]
    mutation: MessageMutation,
    #[serde(rename = "remoteMailbox")]
    remote_mailbox: String,
}

/// Storage writes this payload together with the local optimistic update so
/// the previous flag values cannot be stale by the time a worker needs them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AtomicFlagMutationPayload {
    remote_mailbox: String,
    previous_read: bool,
    previous_flagged: bool,
    is_read: Option<bool>,
    is_flagged: Option<bool>,
}

#[derive(Debug, Clone)]
struct ImmutableMutationTarget {
    remote_mailbox: String,
    uid: u32,
    uid_validity: i64,
}

impl MessageMutation {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => READ_MUTATION_KIND,
            Self::Star { .. } => STAR_MUTATION_KIND,
            Self::MailboxAction { .. } => MAILBOX_ACTION_KIND,
        }
    }
}

/// Stages a durable read-status operation and returns the record that fences
/// delayed background writers. The caller applies the matching optimistic
/// local change after this succeeds.
pub async fn enqueue_read_mutation(
    store: &Store,
    message: &MailSummary,
    read: bool,
    dependency_id: Option<&str>,
) -> Result<OperationJournalEntry> {
    enqueue_and_apply_flag_mutation(
        store,
        message,
        READ_MUTATION_KIND,
        Some(read),
        None,
        dependency_id,
    )
    .await
}

/// Stages a durable star-status operation and returns the record that fences
/// delayed background writers. The caller applies the matching optimistic
/// local change after this succeeds.
pub async fn enqueue_star_mutation(
    store: &Store,
    message: &MailSummary,
    starred: bool,
    dependency_id: Option<&str>,
) -> Result<OperationJournalEntry> {
    enqueue_and_apply_flag_mutation(
        store,
        message,
        STAR_MUTATION_KIND,
        None,
        Some(starred),
        dependency_id,
    )
    .await
}

/// Atomically stages and applies a durable move, trash, spam, or delete
/// intent. The source row moves into an operation-owned hidden membership,
/// with a full backup for rollback, before the command returns to the UI.
pub async fn enqueue_mailbox_action(
    store: &Store,
    message: &MailSummary,
    action: MailboxAction,
    dependency_id: Option<&str>,
) -> Result<OperationJournalEntry> {
    let identity = store
        .capture_message_remote_identity(&message.id)
        .await?
        .ok_or_else(|| anyhow!("message has no committed remote locator"))?;
    enqueue_mailbox_action_for_identity(store, &identity, action, dependency_id).await
}

/// Queues a mailbox action against an identity captured by the provider
/// discovery that selected the message. Sender cleanup must use this rather
/// than resolving the message ID again after its remote scan: an account
/// settings change could otherwise make the same local row appear current in
/// a different provider namespace.
pub async fn enqueue_mailbox_action_for_identity(
    store: &Store,
    identity: &dakia_core::storage::MessageRemoteIdentity,
    action: MailboxAction,
    dependency_id: Option<&str>,
) -> Result<OperationJournalEntry> {
    store
        .enqueue_and_apply_mailbox_action_for_identity(&identity, action, dependency_id)
        .await
}

/// Decodes a mutation worker's payload and rejects a journal row whose kind
/// does not match its payload. This prevents a malformed row from being sent
/// to an unrelated provider action.
pub fn message_mutation_from_operation(entry: &OperationJournalEntry) -> Result<MessageMutation> {
    let journal = journal_message_mutation(entry)?;
    if entry.kind != journal.mutation.kind() {
        return Err(anyhow!(
            "operation kind {} does not match mutation payload {}",
            entry.kind,
            journal.mutation.kind()
        ));
    }
    if journal.remote_mailbox.trim().is_empty() {
        return Err(anyhow!("operation journal mutation has no remote mailbox"));
    }
    Ok(journal.mutation)
}

fn journal_message_mutation(entry: &OperationJournalEntry) -> Result<JournalMessageMutation> {
    if let Ok(journal) = serde_json::from_str::<JournalMessageMutation>(&entry.payload_json) {
        return Ok(journal);
    }
    let payload: AtomicFlagMutationPayload = serde_json::from_str(&entry.payload_json)
        .context("operation journal mutation payload is invalid")?;
    let mutation = match entry.kind.as_str() {
        READ_MUTATION_KIND => MessageMutation::Read {
            value: payload
                .is_read
                .ok_or_else(|| anyhow!("read mutation payload has no desired value"))?,
            previous: payload.previous_read,
        },
        STAR_MUTATION_KIND => MessageMutation::Star {
            value: payload
                .is_flagged
                .ok_or_else(|| anyhow!("star mutation payload has no desired value"))?,
            previous: payload.previous_flagged,
        },
        _ => {
            return Err(anyhow!(
                "operation kind does not match flag mutation payload"
            ))
        }
    };
    Ok(JournalMessageMutation {
        mutation,
        remote_mailbox: payload.remote_mailbox,
    })
}

async fn enqueue_and_apply_flag_mutation(
    store: &Store,
    message: &MailSummary,
    kind: &str,
    is_read: Option<bool>,
    is_flagged: Option<bool>,
    dependency_id: Option<&str>,
) -> Result<OperationJournalEntry> {
    let identity = store
        .capture_message_remote_identity(&message.id)
        .await?
        .ok_or_else(|| anyhow!("message has no committed remote locator"))?;
    store
        .enqueue_and_apply_flag_mutation_for_identity(
            &identity,
            kind,
            is_read,
            is_flagged,
            dependency_id,
        )
        .await
}

/// Drains at most `limit` due message mutations for one account. It never
/// claims SMTP or Sent-copy rows: candidates are selected first, then each
/// message operation is claimed by ID with storage's compare-and-swap lease.
///
/// Call this after account startup or reconnect. Interrupted `submitting`
/// work must first be moved to `uncertain` during process recovery, before a
/// new worker begins, so a live attempt is never mistaken for a retry.
pub async fn drain_message_mutations(
    store: &Store,
    account: &Account,
    claim_owner: &str,
    limit: usize,
) -> Result<MutationDrainReport> {
    if claim_owner.trim().is_empty() {
        return Err(anyhow!("operation claim owner is required"));
    }
    if limit == 0 {
        return Ok(MutationDrainReport::default());
    }

    let mut report = MutationDrainReport::default();
    let candidates = store.pending_operations(account.id).await?;
    for candidate in candidates
        .into_iter()
        .filter(|entry| is_message_mutation_kind(&entry.kind))
        .take(limit)
    {
        let Some(operation) = store
            .claim_operation_by_id(&candidate.operation_id, claim_owner)
            .await?
        else {
            continue;
        };
        report.claimed += 1;
        let _claim_heartbeat = ClaimHeartbeat::start(
            store.clone(),
            operation.operation_id.clone(),
            claim_owner.to_owned(),
        );
        match execute_claimed_message_mutation(store, account, claim_owner, &operation).await? {
            ClaimedMutationOutcome::Completed => report.completed += 1,
            ClaimedMutationOutcome::Retried => report.retried += 1,
            ClaimedMutationOutcome::RolledBack => report.rolled_back += 1,
            ClaimedMutationOutcome::Unresolved => report.unresolved += 1,
        }
        if let Some(updated) = store
            .operation_journal_entry(&operation.operation_id)
            .await?
        {
            report.updates.push(MutationOperationUpdate {
                operation_id: updated.operation_id,
                account_id: updated.account_id,
                message_id: updated.message_id,
                kind: updated.kind,
                status: updated.state,
                error: updated.error,
            });
        }
    }
    Ok(report)
}

/// Converts operations left `submitting` by a previous process into explicit
/// uncertainty before resuming account work. This is a cold-start recovery
/// operation: do not call it while another live process may be sending or
/// mutating the same account.
pub async fn recover_interrupted_account_operations(
    store: &Store,
    account_id: Uuid,
) -> Result<u64> {
    store
        .mark_interrupted_operations_uncertain(account_id)
        .await
}

enum ClaimedMutationOutcome {
    Completed,
    Retried,
    RolledBack,
    Unresolved,
}

async fn execute_claimed_message_mutation(
    store: &Store,
    account: &Account,
    claim_owner: &str,
    operation: &OperationJournalEntry,
) -> Result<ClaimedMutationOutcome> {
    let mutation = message_mutation_from_operation(operation)?;
    let target = immutable_target_from_operation(operation)?;
    if operation.account_id != account.id.to_string() {
        return mark_claimed_unresolved(
            store,
            operation,
            claim_owner,
            "operation account does not match worker account",
        )
        .await;
    }

    let service = MailService::new(store.clone());
    let remote_result = match &mutation {
        MessageMutation::Read { value, .. } => service
            .set_read_with_expected_uidvalidity(
                account,
                &target.remote_mailbox,
                target.uid,
                *value,
                target.uid_validity,
            )
            .await
            .map(|()| None),
        MessageMutation::Star { value, .. } => service
            .set_flagged_with_expected_uidvalidity(
                account,
                &target.remote_mailbox,
                target.uid,
                *value,
                target.uid_validity,
            )
            .await
            .map(|()| None),
        MessageMutation::MailboxAction { action } => {
            service
                .apply_action_with_expected_uidvalidity(
                    account,
                    &target.remote_mailbox,
                    target.uid,
                    *action,
                    target.uid_validity,
                )
                .await
        }
    };

    match remote_result {
        Ok(destination_uid) => {
            if let MessageMutation::MailboxAction { action } = mutation {
                let committed = store
                    .reconcile_and_complete_claimed_mailbox_action(
                        &operation.operation_id,
                        claim_owner,
                        mailbox_action_destination(action).unwrap_or_default(),
                        destination_uid,
                    )
                    .await?;
                if !committed {
                    return mark_claimed_unresolved(
                        store,
                        operation,
                        claim_owner,
                        "the message changed before its remote mailbox action could be committed",
                    )
                    .await;
                }
            } else {
                // Terminal completion releases the matching mutation fence in
                // its own transaction. Flag operations need no membership
                // projection, while mailbox actions use the atomic storage
                // reconciliation above.
                store
                    .complete_claimed_operation(
                        &operation.operation_id,
                        claim_owner,
                        "completed",
                        Some("remote_reconciled"),
                        None,
                        None,
                    )
                    .await?;
            }
            Ok(ClaimedMutationOutcome::Completed)
        }
        Err(error) => {
            let error_text = sanitized_operation_error(&error);
            // The core preserves the IMAP command phase. Only a mailbox
            // action that passed its side-effect boundary can be ambiguous;
            // connection, authentication, SELECT and UIDVALIDITY failures
            // are still safe retry candidates.
            let failure = if matches!(mutation, MessageMutation::MailboxAction { .. })
                && mailbox_action_outcome_is_uncertain(&error)
            {
                RemoteFailure::Uncertain
            } else {
                classify_remote_failure(&error_text)
            };
            match failure {
                RemoteFailure::Retryable => {
                    store
                        .complete_claimed_operation(
                            &operation.operation_id,
                            claim_owner,
                            "retry",
                            Some("remote_retry_scheduled"),
                            Some(&error_text),
                            Some(next_retry_at(operation)),
                        )
                        .await?;
                    Ok(ClaimedMutationOutcome::Retried)
                }
                RemoteFailure::Permanent => {
                    let rolled_back = match mutation {
                        MessageMutation::Read { previous, .. } => {
                            store
                                .rollback_and_complete_claimed_message_flags(
                                    &operation.operation_id,
                                    claim_owner,
                                    Some(previous),
                                    None,
                                    Some("local_rollback_completed"),
                                    Some(&error_text),
                                )
                                .await?
                        }
                        MessageMutation::Star { previous, .. } => {
                            store
                                .rollback_and_complete_claimed_message_flags(
                                    &operation.operation_id,
                                    claim_owner,
                                    None,
                                    Some(previous),
                                    Some("local_rollback_completed"),
                                    Some(&error_text),
                                )
                                .await?
                        }
                        MessageMutation::MailboxAction { .. } => {
                            store
                                .rollback_and_complete_claimed_mailbox_action(
                                    &operation.operation_id,
                                    claim_owner,
                                    &error_text,
                                )
                                .await?
                        }
                    };
                    if !rolled_back {
                        // The rollback helper finalized the journal even
                        // when a newer operation or namespace made the local
                        // write inapplicable. Preserve that fact for UI
                        // feedback without reviving a stale claim.
                        tracing::info!(operation_id = %operation.operation_id, "message mutation rollback was superseded");
                    }
                    Ok(ClaimedMutationOutcome::RolledBack)
                }
                RemoteFailure::Uncertain => {
                    mark_claimed_unresolved(store, operation, claim_owner, &error_text).await
                }
            }
        }
    }
}

async fn mark_claimed_unresolved(
    store: &Store,
    operation: &OperationJournalEntry,
    claim_owner: &str,
    error: &str,
) -> Result<ClaimedMutationOutcome> {
    store
        .complete_claimed_operation(
            &operation.operation_id,
            claim_owner,
            "uncertain",
            Some("remote_outcome_uncertain"),
            Some(error),
            None,
        )
        .await?;
    Ok(ClaimedMutationOutcome::Unresolved)
}

fn immutable_target_from_operation(
    operation: &OperationJournalEntry,
) -> Result<ImmutableMutationTarget> {
    let journal = journal_message_mutation(operation)?;
    if operation.kind != journal.mutation.kind() {
        return Err(anyhow!("operation kind does not match mutation payload"));
    }
    let uid = operation
        .uid
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| anyhow!("operation has an invalid UID locator"))?;
    let uid_validity = operation
        .uid_validity
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow!("operation has an invalid UIDVALIDITY locator"))?;
    if journal.remote_mailbox.trim().is_empty() {
        return Err(anyhow!("operation has no immutable remote mailbox locator"));
    }
    Ok(ImmutableMutationTarget {
        remote_mailbox: journal.remote_mailbox,
        uid,
        uid_validity,
    })
}

fn is_message_mutation_kind(kind: &str) -> bool {
    matches!(
        kind,
        READ_MUTATION_KIND | STAR_MUTATION_KIND | MAILBOX_ACTION_KIND
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteFailure {
    Retryable,
    Permanent,
    Uncertain,
}

fn classify_remote_failure(error: &str) -> RemoteFailure {
    let error = error.to_ascii_lowercase();
    if error.contains("uidvalidity")
        || error.contains("mailbox identity changed")
        || error.contains("recycled uid")
    {
        return RemoteFailure::Uncertain;
    }
    if [
        "timeout",
        "timed out",
        "temporary",
        "try again",
        "connection reset",
        "connection refused",
        "network is unreachable",
        "broken pipe",
        "rate limit",
        "throttl",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return RemoteFailure::Retryable;
    }
    if [
        "permission denied",
        "not permitted",
        "authentication failed",
        "invalid credentials",
        "[noperm]",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return RemoteFailure::Permanent;
    }
    RemoteFailure::Uncertain
}

fn next_retry_at(operation: &OperationJournalEntry) -> DateTime<Utc> {
    let exponent = u32::try_from(operation.attempts.clamp(0, 6)).unwrap_or(6);
    let base_seconds = 5_i64.saturating_mul(1_i64 << exponent).min(300);
    let jitter_milliseconds = operation
        .operation_id
        .bytes()
        .fold(0_i64, |total, byte| total.saturating_add(i64::from(byte)))
        % 1_000;
    Utc::now()
        + chrono::Duration::seconds(base_seconds)
        + chrono::Duration::milliseconds(jitter_milliseconds)
}

fn sanitized_operation_error(error: &anyhow::Error) -> String {
    // Provider errors are already surfaced to the user by command handlers.
    // Keep the journal diagnostic bounded so a malformed server response
    // cannot grow durable state without limit.
    let text = error.to_string();
    text.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: Uuid::new_v4(),
            email: "mutation@example.test".into(),
            account_name: "Mutation".into(),
            display_name: "Mutation".into(),
            provider_id: "fastmail".into(),
            auth: dakia_core::AccountAuth::Password {
                username: "mutation@example.test".into(),
            },
            imap_host: "imap.example.test".into(),
            imap_port: 993,
            imap_security: dakia_core::provider::Security::Tls,
            smtp_host: "smtp.example.test".into(),
            smtp_port: 465,
            smtp_security: dakia_core::provider::Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        }
    }

    fn summary(account: &Account) -> MailSummary {
        MailSummary {
            id: "sender-discovery-message".into(),
            account_id: account.id.to_string(),
            mailbox: "INBOX".into(),
            uid: 7,
            message_id: Some("<sender-discovery@example.test>".into()),
            in_reply_to: None,
            reference_ids: None,
            thread_id: "sender-discovery-thread".into(),
            subject: "Sender discovery".into(),
            from_name: Some("Sender".into()),
            from_address: "sender@example.test".into(),
            to_addresses: account.email.clone(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: "body".into(),
            body_text: "body".into(),
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

    #[test]
    fn mutation_payload_round_trips_with_its_kind() {
        let mutation = MessageMutation::Read {
            value: true,
            previous: false,
        };
        let payload = serde_json::to_string(&mutation).expect("serialize mutation");
        let decoded: MessageMutation =
            serde_json::from_str(&payload).expect("deserialize mutation");

        assert!(matches!(
            decoded,
            MessageMutation::Read {
                value: true,
                previous: false
            }
        ));
        assert_eq!(mutation.kind(), READ_MUTATION_KIND);
    }

    #[test]
    fn each_mutation_uses_a_distinct_journal_kind() {
        assert_eq!(
            MessageMutation::Star {
                value: true,
                previous: false,
            }
            .kind(),
            STAR_MUTATION_KIND
        );
        assert_eq!(
            MessageMutation::MailboxAction {
                action: MailboxAction::Trash,
            }
            .kind(),
            MAILBOX_ACTION_KIND
        );
    }

    #[test]
    fn journal_payload_keeps_the_provider_mailbox_immutable() {
        let payload = JournalMessageMutation {
            mutation: MessageMutation::Read {
                value: true,
                previous: false,
            },
            remote_mailbox: "[Gmail]/All Mail".into(),
        };
        let encoded = serde_json::to_string(&payload).expect("serialize journal payload");
        let decoded: JournalMessageMutation =
            serde_json::from_str(&encoded).expect("deserialize journal payload");

        assert_eq!(decoded.remote_mailbox, "[Gmail]/All Mail");
        assert!(matches!(decoded.mutation, MessageMutation::Read { .. }));
    }

    #[test]
    fn only_known_transient_failures_are_retried() {
        assert_eq!(
            classify_remote_failure("temporary backend timeout"),
            RemoteFailure::Retryable
        );
        assert_eq!(
            classify_remote_failure("mailbox identity changed before STORE"),
            RemoteFailure::Uncertain
        );
        assert_eq!(
            classify_remote_failure("unrecognised provider failure"),
            RemoteFailure::Uncertain
        );
        assert_eq!(
            classify_remote_failure("NO [NOPERM] cannot update flags"),
            RemoteFailure::Permanent
        );
    }

    #[tokio::test]
    async fn supplied_sender_identity_rejects_an_account_config_change_before_enqueue() {
        let store = Store::in_memory().await.unwrap();
        let account = account();
        store.save_account(&account).await.unwrap();
        let message = summary(&account);
        store
            .upsert_catalog_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        store
            .ensure_mailbox_catalog_identity(account.id, "INBOX", "INBOX", 9)
            .await
            .unwrap();
        let discovered = store
            .capture_message_remote_identity(&message.id)
            .await
            .unwrap()
            .expect("discovery receipt");

        let mut changed = account.clone();
        changed.imap_host = "new-imap.example.test".into();
        store.save_account(&changed).await.unwrap();

        let error =
            enqueue_mailbox_action_for_identity(&store, &discovered, MailboxAction::Trash, None)
                .await
                .unwrap_err();
        assert!(error.to_string().contains("stale"));
        assert!(store
            .pending_operations(account.id)
            .await
            .unwrap()
            .is_empty());
    }
}
