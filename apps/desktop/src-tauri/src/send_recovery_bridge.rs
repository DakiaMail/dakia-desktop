//! Tauri authority boundary for durable send recovery.
//!
//! The database owns recoverable message content. This module only keeps a
//! short-lived, exact-window capability for a Compose child to read one record.

use super::{enabled_account_for_operation, error, open_mail_window, AppState, MailWindowSpec};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use dakia_core::{
    mail::validate_compose_message_for_account,
    storage::{SendRecoveryCursor, SendRecoveryPage, SendRecoveryStatus},
    ComposeMessage,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
#[cfg(test)]
use tauri::Listener;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use uuid::Uuid;

const MAX_RECOVERY_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecoveryIdentity {
    account_id: Uuid,
    attempt_id: Uuid,
}

#[derive(Debug)]
struct SendRecoveryGrant {
    label: String,
    identity: RecoveryIdentity,
    acknowledged: bool,
    opening: bool,
}

#[derive(Default)]
struct SendRecoveryGrants {
    entries: HashMap<Uuid, SendRecoveryGrant>,
    identities: HashMap<RecoveryIdentity, Uuid>,
}

enum GrantReservation {
    Issued,
    ExistingWindow(String),
}

/// Runtime-only state. A record remains in SQLite after this state is lost, so
/// an interrupted child can always be opened again from the main window.
#[derive(Default)]
pub(crate) struct SendRecoveryRuntime {
    /// `false` reserves an ID before its recovery row exists; `true` means a
    /// durable recovery row already exists and must be omitted from its
    /// sender's Outbox.
    active: Mutex<HashMap<RecoveryIdentity, bool>>,
    grants: Mutex<SendRecoveryGrants>,
}

pub(crate) struct ActiveSendRecovery<R: tauri::Runtime> {
    runtime: Arc<SendRecoveryRuntime>,
    identity: RecoveryIdentity,
    app: AppHandle<R>,
}

impl SendRecoveryRuntime {
    pub(crate) fn reserve<R: tauri::Runtime>(
        self: &Arc<Self>,
        account_id: Uuid,
        attempt_id: Uuid,
        app: AppHandle<R>,
        existing_recovery: bool,
    ) -> Result<ActiveSendRecovery<R>, String> {
        let identity = RecoveryIdentity {
            account_id,
            attempt_id,
        };
        let mut active = self.active.lock().map_err(error)?;
        if active.contains_key(&identity) {
            return Err("This message is already being submitted".into());
        }
        active.insert(identity, existing_recovery);
        Ok(ActiveSendRecovery {
            runtime: Arc::clone(self),
            identity,
            app,
        })
    }

    #[cfg(test)]
    fn active_persisted(&self) -> Result<HashSet<RecoveryIdentity>, String> {
        Ok(self.active_snapshot()?.1)
    }

    #[cfg(test)]
    fn active_identities(&self) -> Result<HashSet<RecoveryIdentity>, String> {
        Ok(self.active_snapshot()?.0)
    }

    fn active_snapshot(
        &self,
    ) -> Result<(HashSet<RecoveryIdentity>, HashSet<RecoveryIdentity>), String> {
        let active = self.active.lock().map_err(error)?;
        let all = active.keys().copied().collect();
        let durable = active
            .iter()
            .filter_map(|(identity, persisted)| persisted.then_some(*identity))
            .collect();
        Ok((all, durable))
    }

    fn reserve_grant(
        &self,
        key: Uuid,
        label: String,
        account_id: Uuid,
        attempt_id: Uuid,
        window_exists: impl Fn(&str) -> bool,
    ) -> Result<GrantReservation, String> {
        let identity = RecoveryIdentity {
            account_id,
            attempt_id,
        };
        let mut grants = self.grants.lock().map_err(error)?;
        if let Some(existing_key) = grants.identities.get(&identity).copied() {
            let existing = grants
                .entries
                .get(&existing_key)
                .ok_or_else(|| "Send recovery grant is invalid".to_owned())?;
            if existing.opening {
                return Err("Saved message is already opening".into());
            }
            if window_exists(&existing.label) {
                return Ok(GrantReservation::ExistingWindow(existing.label.clone()));
            }
            grants.entries.remove(&existing_key);
            grants.identities.remove(&identity);
        }
        grants.entries.insert(
            key,
            SendRecoveryGrant {
                label,
                identity,
                acknowledged: false,
                opening: true,
            },
        );
        grants.identities.insert(identity, key);
        Ok(GrantReservation::Issued)
    }

    fn mark_window_open(&self, key: Uuid) -> Result<(), String> {
        let mut grants = self.grants.lock().map_err(error)?;
        let grant = grants
            .entries
            .get_mut(&key)
            .ok_or_else(|| "Send recovery is no longer available".to_owned())?;
        grant.opening = false;
        Ok(())
    }

    fn grant_for(&self, key: Uuid, label: &str) -> Result<RecoveryIdentity, String> {
        let grants = self.grants.lock().map_err(error)?;
        let grant = grants
            .entries
            .get(&key)
            .ok_or_else(|| "Send recovery is no longer available".to_owned())?;
        if grant.label != label || grant.acknowledged || grant.opening {
            return Err("Send recovery is not available to this window".into());
        }
        Ok(grant.identity)
    }

    fn acknowledge(&self, key: Uuid, label: &str) -> Result<(), String> {
        let mut grants = self.grants.lock().map_err(error)?;
        let grant = grants
            .entries
            .get_mut(&key)
            .ok_or_else(|| "Send recovery is no longer available".to_owned())?;
        if grant.label != label || grant.acknowledged || grant.opening {
            return Err("Send recovery is not available to this window".into());
        }
        grant.acknowledged = true;
        Ok(())
    }

    fn revoke(&self, key: Uuid) {
        if let Ok(mut grants) = self.grants.lock() {
            if let Some(grant) = grants.entries.remove(&key) {
                grants.identities.remove(&grant.identity);
            }
        }
    }

    pub(crate) fn revoke_window(&self, label: &str) {
        if let Ok(mut grants) = self.grants.lock() {
            let keys = grants
                .entries
                .iter()
                .filter_map(|(key, grant)| (grant.label == label).then_some(*key))
                .collect::<Vec<_>>();
            for key in keys {
                if let Some(grant) = grants.entries.remove(&key) {
                    grants.identities.remove(&grant.identity);
                }
            }
        }
    }
}

impl<R: tauri::Runtime> ActiveSendRecovery<R> {
    pub(crate) fn mark_persisted(&self) -> Result<(), String> {
        let mut active = self.runtime.active.lock().map_err(error)?;
        let entry = active
            .get_mut(&self.identity)
            .ok_or_else(|| "Send recovery reservation was lost".to_owned())?;
        *entry = true;
        Ok(())
    }
}

impl<R: tauri::Runtime> Drop for ActiveSendRecovery<R> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.runtime.active.lock() {
            active.remove(&self.identity);
        }
        // This closes an optimistic Outbox row even when the compose WebView
        // closes while its native command is still running. It carries only a
        // stable client attempt ID, never mail content.
        let _ = self.app.emit_to(
            "main",
            "outbox-changed",
            serde_json::json!({
                "phase": "finished",
                "id": outbox_item_id(self.identity),
            }),
        );
    }
}

fn outbox_item_id(identity: RecoveryIdentity) -> String {
    format!("outbox-{}-{}", identity.account_id, identity.attempt_id)
}

#[derive(Serialize, Deserialize)]
struct RecoveryCursorEnvelope {
    account_ids: Vec<Uuid>,
    cursor: SendRecoveryCursor,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct RecoveryListPage {
    items: Vec<dakia_core::storage::SendRecoverySummary>,
    next_cursor: Option<String>,
    total_count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ComposeRecoverySeed {
    account_id: Uuid,
    attempt_id: Uuid,
    status: SendRecoveryStatus,
    message: RecoveryComposeMessage,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryComposeMessage {
    account_id: Uuid,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body_text: String,
    body_html: Option<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
    attachments: Vec<RecoveryComposeAttachment>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryComposeAttachment {
    filename: String,
    mime_type: String,
    content_base64: String,
    size_bytes: u64,
}

fn normalized_account_ids(mut account_ids: Vec<Uuid>) -> Vec<Uuid> {
    account_ids.sort_unstable();
    account_ids.dedup();
    account_ids
}

fn encode_cursor(account_ids: &[Uuid], cursor: &SendRecoveryCursor) -> Result<String, String> {
    let payload = serde_json::to_vec(&RecoveryCursorEnvelope {
        account_ids: account_ids.to_vec(),
        cursor: cursor.clone(),
    })
    .map_err(error)?;
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

fn decode_cursor(account_ids: &[Uuid], value: &str) -> Result<SendRecoveryCursor, String> {
    let encoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "Send recovery cursor is invalid".to_owned())?;
    let envelope: RecoveryCursorEnvelope = serde_json::from_slice(&encoded)
        .map_err(|_| "Send recovery cursor is invalid".to_owned())?;
    if envelope.account_ids != account_ids {
        return Err("Send recovery cursor does not match these accounts".into());
    }
    Ok(envelope.cursor)
}

fn compose_seed_message(message: ComposeMessage) -> Result<RecoveryComposeMessage, String> {
    let mut total = 0usize;
    let attachments = message
        .attachments
        .into_iter()
        .map(|attachment| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&attachment.content_base64)
                .map_err(|_| "Saved message attachment data is invalid".to_owned())?;
            total = total
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_RECOVERY_ATTACHMENT_BYTES)
                .ok_or_else(|| "Saved message attachment data is too large".to_owned())?;
            Ok(RecoveryComposeAttachment {
                filename: attachment.filename,
                mime_type: attachment.mime_type,
                content_base64: attachment.content_base64,
                size_bytes: u64::try_from(bytes.len())
                    .map_err(|_| "Saved message attachment data is too large".to_owned())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RecoveryComposeMessage {
        account_id: message.account_id,
        to: message.to,
        cc: message.cc,
        bcc: message.bcc,
        subject: message.subject,
        body_text: message.body_text,
        body_html: message.body_html,
        in_reply_to: message.in_reply_to,
        references: message.references,
        attachments,
    })
}

fn compose_window_label(label: &str) -> bool {
    label
        .strip_prefix("compose-")
        .is_some_and(|id| Uuid::parse_str(id).is_ok())
}

struct FilteredRecoveryPage {
    items: Vec<dakia_core::storage::SendRecoverySummary>,
    next_cursor: Option<SendRecoveryCursor>,
    total_count: u64,
}

fn filter_active_recovery_page(
    page: SendRecoveryPage,
    account_ids: &[Uuid],
    active: &HashSet<RecoveryIdentity>,
    durable_active: &HashSet<RecoveryIdentity>,
) -> FilteredRecoveryPage {
    let active_count = durable_active
        .iter()
        .filter(|identity| account_ids.contains(&identity.account_id))
        .count() as u64;
    let items = page
        .items
        .into_iter()
        .filter(|summary| {
            !active.contains(&RecoveryIdentity {
                account_id: summary.account_id,
                attempt_id: summary.attempt_id,
            })
        })
        .collect();
    FilteredRecoveryPage {
        items,
        next_cursor: page.next_cursor,
        total_count: page.total_count.saturating_sub(active_count),
    }
}

#[tauri::command]
pub(crate) async fn list_send_recoveries(
    state: State<'_, Arc<AppState>>,
    runtime: State<'_, Arc<SendRecoveryRuntime>>,
    account_ids: Vec<Uuid>,
    cursor: Option<String>,
) -> Result<RecoveryListPage, String> {
    let account_ids = normalized_account_ids(account_ids);
    let cursor = cursor
        .as_deref()
        .map(|value| decode_cursor(&account_ids, value))
        .transpose()?;
    let page = state
        .store
        .list_send_recoveries(&account_ids, cursor.as_ref())
        .await
        .map_err(error)?;
    let (active, durable_active) = runtime.active_snapshot()?;
    let page = filter_active_recovery_page(page, &account_ids, &active, &durable_active);
    Ok(RecoveryListPage {
        items: page.items,
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(|next| encode_cursor(&account_ids, next))
            .transpose()?,
        total_count: page.total_count,
    })
}

#[tauri::command]
pub(crate) async fn open_send_recovery_window<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<AppState>>,
    runtime: State<'_, Arc<SendRecoveryRuntime>>,
    account_id: Uuid,
    attempt_id: Uuid,
) -> Result<(), String> {
    let recovery = state
        .store
        .load_send_recovery(account_id, attempt_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Saved message is no longer available".to_owned())?;
    if recovery.account_id != account_id || recovery.attempt_id != attempt_id {
        return Err("Saved message target is invalid".into());
    }
    let label = format!("compose-{}", Uuid::new_v4());
    let key = Uuid::new_v4();
    let path = format!("/?view=compose&recoveryKey={key}&opener=main");
    // A child can begin its bootstrap immediately after the native builder
    // creates it. Reserve its exact label-bound grant first, then revoke it if
    // window creation fails. A concurrent open for the same recovery either
    // focuses the existing child or receives the fixed opening error above.
    match runtime.reserve_grant(key, label.clone(), account_id, attempt_id, |candidate| {
        app.get_webview_window(candidate).is_some()
    })? {
        GrantReservation::ExistingWindow(existing) => {
            let window = app
                .get_webview_window(&existing)
                .ok_or_else(|| "Saved message window is no longer available".to_owned())?;
            window
                .show()
                .and_then(|_| window.set_focus())
                .map_err(error)?;
            return Ok(());
        }
        GrantReservation::Issued => {}
    }
    if let Err(open_error) = open_mail_window(
        &app,
        MailWindowSpec {
            label: label.clone(),
            path,
            title: "New message",
            size: (880.0, 700.0),
            minimum_size: (620.0, 480.0),
        },
    ) {
        runtime.revoke(key);
        return Err(open_error);
    }
    runtime.mark_window_open(key)?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn read_send_recovery_seed<R: tauri::Runtime>(
    state: State<'_, Arc<AppState>>,
    runtime: State<'_, Arc<SendRecoveryRuntime>>,
    window: WebviewWindow<R>,
    recovery_key: String,
) -> Result<ComposeRecoverySeed, String> {
    if !compose_window_label(window.label()) {
        return Err("Send recovery is only available to a Compose window".into());
    }
    let key =
        Uuid::parse_str(&recovery_key).map_err(|_| "Send recovery key is invalid".to_owned())?;
    let identity = runtime.grant_for(key, window.label())?;
    let recovery = state
        .store
        .load_send_recovery(identity.account_id, identity.attempt_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Saved message is no longer available".to_owned())?;
    let account = enabled_account_for_operation(state.inner(), identity.account_id)
        .await
        .map_err(|_| "Saved message account is no longer available".to_owned())?;
    validate_compose_message_for_account(&account, &recovery.message).map_err(error)?;
    Ok(ComposeRecoverySeed {
        account_id: recovery.account_id,
        attempt_id: recovery.attempt_id,
        status: recovery.status,
        message: compose_seed_message(recovery.message)?,
    })
}

#[tauri::command]
pub(crate) fn acknowledge_send_recovery_seed<R: tauri::Runtime>(
    runtime: State<'_, Arc<SendRecoveryRuntime>>,
    window: WebviewWindow<R>,
    recovery_key: String,
) -> Result<(), String> {
    if !compose_window_label(window.label()) {
        return Err("Send recovery is only available to a Compose window".into());
    }
    let key =
        Uuid::parse_str(&recovery_key).map_err(|_| "Send recovery key is invalid".to_owned())?;
    runtime.acknowledge(key, window.label())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dakia_core::{provider, AccountDraft, EmailClassificationInput, Store};

    struct NeverClassifier;

    impl super::super::EmailClassifier for NeverClassifier {
        fn classify(
            &mut self,
            _emails: &[EmailClassificationInput],
        ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
            panic!("send recovery preflight must not classify mail")
        }
    }

    fn test_account() -> dakia_core::Account {
        AccountDraft {
            email: "recovery@example.test".into(),
            display_name: "Recovery test".into(),
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
        .into_account(provider::by_id("custom").expect("custom provider"))
    }

    fn test_state(store: Store) -> Arc<super::super::AppState> {
        Arc::new(super::super::AppState {
            realtime: super::super::RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: std::path::PathBuf::new(),
            classifier: Mutex::new(Box::new(NeverClassifier)),
            classification_owner: "send-recovery-bridge-test".into(),
            classification: Arc::new(super::super::ClassificationScheduler::default()),
            remote_operation_slots: Arc::new(tokio::sync::Semaphore::new(
                super::super::MESSAGE_HYDRATION_CONCURRENCY,
            )),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: super::super::MailRebuildCancellations::default(),
            account_operations: super::super::AccountOperationLocks::default(),
            translation_downloads: Mutex::new(HashMap::new()),
        })
    }

    fn draft(account_id: Uuid) -> ComposeMessage {
        ComposeMessage {
            account_id,
            to: vec!["recipient@example.test".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Durable recovery".into(),
            body_text: "body".into(),
            body_html: None,
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn grant_is_exact_window_bound_and_acknowledgement_consumes_it() {
        let runtime = SendRecoveryRuntime::default();
        let key = Uuid::new_v4();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let label = format!("compose-{}", Uuid::new_v4());
        assert!(matches!(
            runtime
                .reserve_grant(key, label.clone(), account_id, attempt_id, |_| false)
                .expect("issue grant before child creation"),
            GrantReservation::Issued
        ));
        runtime.mark_window_open(key).expect("window built");
        assert_eq!(
            runtime
                .grant_for(key, &label)
                .expect("exact label may read"),
            RecoveryIdentity {
                account_id,
                attempt_id
            }
        );
        assert!(runtime
            .grant_for(key, "compose-00000000-0000-0000-0000-000000000000")
            .is_err());
        runtime
            .acknowledge(key, &label)
            .expect("child acknowledgement");
        assert!(runtime.grant_for(key, &label).is_err());
    }

    #[test]
    fn revoked_preissued_grant_cannot_be_read_after_window_creation_failure() {
        let runtime = SendRecoveryRuntime::default();
        let key = Uuid::new_v4();
        let label = format!("compose-{}", Uuid::new_v4());
        runtime
            .reserve_grant(key, label.clone(), Uuid::new_v4(), Uuid::new_v4(), |_| {
                false
            })
            .expect("grant issued before native build");
        runtime.revoke(key);
        assert!(runtime.grant_for(key, &label).is_err());
    }

    #[test]
    fn destroyed_recovery_window_revokes_its_exact_grant() {
        let runtime = SendRecoveryRuntime::default();
        let key = Uuid::new_v4();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let label = format!("compose-{}", Uuid::new_v4());
        runtime
            .reserve_grant(key, label.clone(), account_id, attempt_id, |_| false)
            .expect("issue grant");
        runtime.mark_window_open(key).expect("window built");
        runtime.revoke_window(&label);
        assert!(runtime.grant_for(key, &label).is_err());
        assert!(matches!(
            runtime
                .reserve_grant(Uuid::new_v4(), label, account_id, attempt_id, |_| false)
                .expect("closed window must not retain its grant"),
            GrantReservation::Issued
        ));
    }

    #[test]
    fn finished_outbox_ids_keep_same_attempts_in_different_accounts_distinct() {
        let attempt_id = Uuid::new_v4();
        let first = outbox_item_id(RecoveryIdentity {
            account_id: Uuid::new_v4(),
            attempt_id,
        });
        let second = outbox_item_id(RecoveryIdentity {
            account_id: Uuid::new_v4(),
            attempt_id,
        });
        assert_ne!(first, second);
        assert!(first.starts_with("outbox-"));
    }

    #[test]
    fn finished_event_uses_the_exact_account_scoped_outbox_id() {
        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let main = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
            .build()
            .expect("main window");
        let (sent, received) = std::sync::mpsc::channel();
        let listener = main.listen("outbox-changed", move |event| {
            sent.send(event.payload().to_owned())
                .expect("deliver terminal outbox event");
        });
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let active = runtime
            .reserve(account_id, attempt_id, app.handle().clone(), false)
            .expect("reserve active send");
        drop(active);
        let payload = received
            .recv_timeout(Duration::from_secs(1))
            .expect("finished event reaches main")
            .parse::<serde_json::Value>()
            .expect("finished event payload JSON");
        assert_eq!(payload["phase"], "finished");
        assert_eq!(
            payload["id"],
            outbox_item_id(RecoveryIdentity {
                account_id,
                attempt_id,
            })
        );
        main.unlisten(listener);
    }

    #[test]
    fn same_recovery_cannot_issue_two_child_grants() {
        let runtime = SendRecoveryRuntime::default();
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let first_key = Uuid::new_v4();
        let first_label = format!("compose-{}", Uuid::new_v4());
        assert!(matches!(
            runtime
                .reserve_grant(
                    first_key,
                    first_label.clone(),
                    account_id,
                    attempt_id,
                    |_| false,
                )
                .expect("first open reserves"),
            GrantReservation::Issued
        ));
        assert!(runtime
            .reserve_grant(
                Uuid::new_v4(),
                format!("compose-{}", Uuid::new_v4()),
                account_id,
                attempt_id,
                |_| false,
            )
            .is_err());
        runtime
            .mark_window_open(first_key)
            .expect("first build completes");
        assert!(matches!(
            runtime
                .reserve_grant(
                    Uuid::new_v4(),
                    format!("compose-{}", Uuid::new_v4()),
                    account_id,
                    attempt_id,
                    |label| label == first_label,
                )
                .expect("existing child is focused"),
            GrantReservation::ExistingWindow(label) if label == first_label
        ));
    }

    #[test]
    fn active_durable_attempt_is_hidden_until_its_guard_drops() {
        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let active = runtime
            .reserve(account_id, attempt_id, app.handle().clone(), false)
            .expect("reserve one active attempt");
        assert!(runtime.active_persisted().expect("active rows").is_empty());
        active.mark_persisted().expect("durable prepare");
        assert!(runtime
            .active_persisted()
            .expect("active rows")
            .contains(&RecoveryIdentity {
                account_id,
                attempt_id
            }));
        drop(active);
        assert!(runtime.active_persisted().expect("active rows").is_empty());
    }

    #[test]
    fn retryable_recovery_is_hidden_before_its_new_claim_is_persisted() {
        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let account_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let identity = RecoveryIdentity {
            account_id,
            attempt_id,
        };
        let active = runtime
            .reserve(account_id, attempt_id, app.handle().clone(), true)
            .expect("reserve an existing retryable recovery");

        let (all, durable) = runtime.active_snapshot().expect("atomic active snapshot");
        assert!(all.contains(&identity));
        assert!(durable.contains(&identity));
        assert!(durable.is_subset(&all));
        assert!(runtime
            .reserve(account_id, attempt_id, app.handle().clone(), true)
            .is_err());

        drop(active);
        assert!(runtime
            .active_identities()
            .expect("all active identities")
            .is_empty());
    }

    #[tokio::test]
    async fn retryable_recovery_is_absent_from_the_list_while_a_resend_is_reserved() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = test_account();
        store.save_account(&account).await.expect("save account");
        let attempt_id = Uuid::new_v4();
        let message = draft(account.id);
        store
            .prepare_send_recovery(account.id, attempt_id, &message)
            .await
            .expect("persist recovery");
        store
            .release_send_recovery(account.id, attempt_id)
            .await
            .expect("make recovery retryable");
        let page = store
            .list_send_recoveries(&[account.id], None)
            .await
            .expect("list retryable recovery");
        assert_eq!(page.items.len(), 1);

        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let active = runtime
            .reserve(account.id, attempt_id, app.handle().clone(), true)
            .expect("reserve retryable recovery before a new durable claim");
        let filtered = filter_active_recovery_page(
            page,
            &[account.id],
            &runtime.active_identities().expect("all active identities"),
            &runtime
                .active_persisted()
                .expect("durable active identities"),
        );

        assert!(filtered.items.is_empty());
        assert_eq!(filtered.total_count, 0);
        drop(active);
    }

    #[tokio::test]
    async fn account_disabled_while_waiting_releases_the_durable_claim_as_not_submitted() {
        let store = Store::in_memory().await.expect("in-memory store");
        let mut account = test_account();
        store.save_account(&account).await.expect("save account");
        let state = test_state(store.clone());
        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let attempt_id = Uuid::new_v4();
        let message = draft(account.id);
        let held_lock = state.account_operations.acquire(account.id).await;
        let task_state = Arc::clone(&state);
        let task_runtime = Arc::clone(&runtime);
        let task_app = app.handle().clone();
        let task = tokio::spawn(async move {
            super::super::submit_recoverable_send_attempt(
                task_state.as_ref(),
                task_runtime,
                task_app,
                &message,
                attempt_id,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if store
                    .load_send_recovery(account.id, attempt_id)
                    .await
                    .expect("load recovery")
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("claim must persist before waiting for account lock");
        account.enabled = false;
        store.save_account(&account).await.expect("disable account");
        drop(held_lock);

        let error = task
            .await
            .expect("send task")
            .expect_err("disabled account must not reach SMTP");
        assert!(error.contains("not_submitted"));
        let recovery = store
            .load_send_recovery(account.id, attempt_id)
            .await
            .expect("load released recovery")
            .expect("not-submitted recovery remains editable");
        assert_eq!(recovery.status, SendRecoveryStatus::NotSubmitted);
        assert!(runtime.active_persisted().expect("active rows").is_empty());
    }

    #[tokio::test]
    async fn initially_disabled_account_retains_a_not_submitted_recovery() {
        let store = Store::in_memory().await.expect("in-memory store");
        let mut account = test_account();
        account.enabled = false;
        store
            .save_account(&account)
            .await
            .expect("save disabled account");
        let state = test_state(store.clone());
        let app = tauri::test::mock_builder()
            .build(super::super::test_support::mock_context())
            .expect("mock app");
        let runtime = Arc::new(SendRecoveryRuntime::default());
        let attempt_id = Uuid::new_v4();
        let error = super::super::submit_recoverable_send_attempt(
            state.as_ref(),
            Arc::clone(&runtime),
            app.handle().clone(),
            &draft(account.id),
            attempt_id,
        )
        .await
        .expect_err("disabled account must not open SMTP");
        assert!(error.contains("not_submitted"));
        let recovery = store
            .load_send_recovery(account.id, attempt_id)
            .await
            .expect("load recovery")
            .expect("failed send draft remains recoverable");
        assert_eq!(recovery.status, SendRecoveryStatus::NotSubmitted);
        assert!(runtime.active_persisted().expect("active rows").is_empty());
    }
}
