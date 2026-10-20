mod realtime;
mod translation;

#[cfg(test)]
mod tauri_contracts_tests;

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
#[cfg(test)]
use dakia_core::storage::SelectableMailboxDraft;
use dakia_core::storage::{ConversationTarget, MessageContentFetchAcquire, SelectableMailbox};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, normalize_sender_address, parse_search_query, provider, Account,
    AccountAuth, AccountDraft, Attachment, CachedMessageContent, ComposeMessage,
    EmailClassificationInput, LocalEmailClassifier, MailConversation, MailConversationPage,
    MailRebuildJob, MailService, MailSummary, MailboxAction, ModelClassificationUpdate,
    ProviderMailboxSearchState, ProviderPreset, SearchContinuationV2, SearchCoverage,
    SearchCoverageState, SearchErrorCategory, SearchErrorV2, SearchExecutionMode, SearchPageV2,
    SearchQuery, SearchRequestV2, SearchSession, SearchSessionRegistry, SenderTrashResult,
    SmartInboxPage, SmartInboxQuery, Store, SyncProgress, SyncResult, UnsubscribeOutcome,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    future::Future,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "macos")]
use std::process::Command;
use tauri::{
    image::Image,
    ipc::Channel,
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    DragDropEvent, Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{watch, Mutex as AsyncMutex, Notify, OwnedMutexGuard, Semaphore};
use url::Url;
use uuid::Uuid;

use realtime::{RealtimeSyncManager, RealtimeSyncStatus};
use translation::{
    TranslationDownloadProgress, TranslationLanguageDetection, TranslationModelFiles,
    TranslationModelStatus,
};

const MESSAGE_HYDRATION_CONCURRENCY: usize = 4;
/// Keep one shared provider-operation slot for opening mail. Searches have a
/// separate global cap, so concurrent search pages cannot consume every
/// connection slot and make an open wait behind a whole result page.
const REMOTE_SEARCH_CONCURRENCY: usize = MESSAGE_HYDRATION_CONCURRENCY - 1;
const CLASSIFICATION_BATCH_SIZE: usize = 64;
const CLASSIFICATION_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(500)];
const MAX_EXPORT_FILENAME_BYTES: usize = 255;
const MAX_DOWNLOAD_COLLISION_SUFFIX_BYTES: usize = " (9999)".len();

struct AppState {
    store: Store,
    data_dir: PathBuf,
    classifier: Mutex<Box<dyn EmailClassifier>>,
    classification_owner: String,
    classification: Arc<ClassificationScheduler>,
    realtime: RealtimeSyncManager,
    remote_operation_slots: Arc<Semaphore>,
    remote_search_slots: Arc<Semaphore>,
    mail_rebuilds: Mutex<HashMap<Uuid, MailRebuildProgress>>,
    mail_rebuild_running: Mutex<HashSet<Uuid>>,
    mail_rebuild_cancellations: MailRebuildCancellations,
    account_operations: AccountOperationLocks,
    search_sessions: SearchSessionRegistry,
    translation_downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Serializes runtime restarts of bounded contacted-people migrations.
    /// Account changes may arrive while an earlier drain is yielding; queued
    /// drains are harmless no-ops once the durable cursor is complete.
    contacted_people_migration_drain: Arc<AsyncMutex<()>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContactedPeopleSettings {
    enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContactedPeopleChanged {
    enabled: bool,
    cleared: bool,
}

/// Incremental coverage is emitted while a submitted hybrid search is still
/// running. The session ID and revision let every window discard progress from
/// an older search before it changes visible coverage.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchProgressUpdate {
    session_id: Uuid,
    revision: u64,
    coverage: Vec<SearchCoverage>,
}

trait EmailClassifier: Send {
    fn revision(&self) -> &str {
        "test-classifier"
    }

    fn classify(
        &mut self,
        emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>>;
}

impl EmailClassifier for LocalEmailClassifier {
    fn revision(&self) -> &str {
        LocalEmailClassifier::revision(self)
    }

    fn classify(
        &mut self,
        emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        LocalEmailClassifier::classify(self, emails)
    }
}

/// Serializes destructive and provider-backed work for one account without
/// unnecessarily blocking other accounts.  A deletion holds this lock from
/// stopping realtime through the storage transaction, so an in-flight manual
/// sync or resumed rebuild cannot finish by repopulating the removed account.
#[derive(Default)]
struct AccountOperationLocks {
    locks: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
}

impl AccountOperationLocks {
    async fn acquire(&self, account_id: Uuid) -> OwnedMutexGuard<()> {
        let operation = self
            .locks
            .lock()
            .expect("account operation lock poisoned")
            .entry(account_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        operation.lock_owned().await
    }
}

/// Coordinates a rebuild cancellation across the brief interval before a
/// queued rebuild obtains the per-account operation lock.
#[derive(Default)]
struct MailRebuildCancellations {
    active: Mutex<HashMap<Uuid, MailRebuildCancellation>>,
}

struct MailRebuildCancellation {
    sender: watch::Sender<MailRebuildCancellationDisposition>,
    registrations: usize,
    reserved: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MailRebuildCancellationDisposition {
    None,
    Retain,
    Replace,
    Remove,
}

impl MailRebuildCancellations {
    fn reserve(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        let cancellation = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(MailRebuildCancellationDisposition::None);
            MailRebuildCancellation {
                sender,
                registrations: 0,
                reserved: false,
            }
        });
        cancellation.reserved = true;
    }

    fn register(&self, account_id: Uuid) -> watch::Receiver<MailRebuildCancellationDisposition> {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        let cancellation = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(MailRebuildCancellationDisposition::None);
            MailRebuildCancellation {
                sender,
                registrations: 0,
                reserved: false,
            }
        });
        cancellation.registrations += 1;
        cancellation.sender.subscribe()
    }

    fn request(&self, account_id: Uuid, disposition: MailRebuildCancellationDisposition) {
        if let Some(sender) = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned")
            .get(&account_id)
            .map(|cancellation| cancellation.sender.clone())
        {
            let current = *sender.borrow();
            let next = match (current, disposition) {
                (MailRebuildCancellationDisposition::Remove, _)
                | (_, MailRebuildCancellationDisposition::Remove) => {
                    MailRebuildCancellationDisposition::Remove
                }
                (MailRebuildCancellationDisposition::Replace, _)
                | (_, MailRebuildCancellationDisposition::Replace) => {
                    MailRebuildCancellationDisposition::Replace
                }
                (MailRebuildCancellationDisposition::Retain, _)
                | (_, MailRebuildCancellationDisposition::Retain) => {
                    MailRebuildCancellationDisposition::Retain
                }
                _ => MailRebuildCancellationDisposition::None,
            };
            // `send` drops the update when a reservation has no subscribed
            // task yet. `send_replace` records it for the later register.
            sender.send_replace(next);
        }
    }

    fn disposition(
        &self,
        receiver: &watch::Receiver<MailRebuildCancellationDisposition>,
    ) -> MailRebuildCancellationDisposition {
        *receiver.borrow()
    }

    fn clear(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        if let Some(cancellation) = active.get_mut(&account_id) {
            cancellation.registrations -= 1;
            if cancellation.registrations == 0 && !cancellation.reserved {
                active.remove(&account_id);
            }
        }
    }

    fn release_reservation(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        if let Some(cancellation) = active.get_mut(&account_id) {
            cancellation.reserved = false;
            if cancellation.registrations == 0 {
                active.remove(&account_id);
            }
        }
    }
}

fn normalized_account_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn matching_account_email(account: &Account, email: &str) -> bool {
    normalized_account_email(&account.email) == normalized_account_email(email)
}

fn same_mail_namespace(existing: &Account, candidate: &Account) -> bool {
    existing.provider_id == candidate.provider_id
        && match (&existing.auth, &candidate.auth) {
            (
                AccountAuth::Password {
                    username: existing_username,
                },
                AccountAuth::Password {
                    username: candidate_username,
                },
            ) => existing_username == candidate_username,
            (
                AccountAuth::OAuth2 {
                    username: existing_username,
                    provider: existing_provider,
                    ..
                },
                AccountAuth::OAuth2 {
                    username: candidate_username,
                    provider: candidate_provider,
                    ..
                },
            ) => existing_username == candidate_username && existing_provider == candidate_provider,
            _ => false,
        }
        && existing
            .imap_host
            .trim()
            .eq_ignore_ascii_case(candidate.imap_host.trim())
        && existing.imap_port == candidate.imap_port
        && existing.imap_security == candidate.imap_security
        && existing.archive_mailbox == candidate.archive_mailbox
        && existing.spam_mailbox == candidate.spam_mailbox
}

fn credential_secret_name(account: &Account) -> String {
    // Keep this aligned with `dakia_core::mail::CredentialStore::key` so a
    // failed account save can restore a credential it just replaced.
    format!("dev.dakia.mail:{}:{}", account.id, account.auth.username())
}

/// Converts only the authentication scheme. Callers retain the same account
/// value so mailbox settings, ID, and indexed data remain associated with it.
fn convert_legacy_oauth_to_password(account: &mut Account) -> bool {
    let AccountAuth::OAuth2 { username, .. } = &account.auth else {
        return false;
    };
    account.auth = AccountAuth::Password {
        username: username.clone(),
    };
    true
}

fn validate_legacy_oauth_conversion(
    account: &Account,
    statuses: &[RealtimeSyncStatus],
) -> Result<(), String> {
    let AccountAuth::OAuth2 { provider, .. } = &account.auth else {
        return Ok(());
    };
    if account.provider_id != "gmail" || provider != "gmail" {
        return Err("Only legacy Gmail OAuth accounts can be converted to an app password".into());
    }
    let failed_authentication = statuses.iter().any(|status| {
        status.account_id == account.id
            && status.state == "paused"
            && status.error_kind.as_deref() == Some("authentication")
    });
    if !failed_authentication {
        return Err(
            "This Gmail account can use an app password after its existing sign-in stops working"
                .into(),
        );
    }
    Ok(())
}

fn ensure_account_is_not_connected(accounts: &[Account], email: &str) -> Result<(), String> {
    if accounts
        .iter()
        .any(|stored| matching_account_email(stored, email))
    {
        return Err("This account is already connected. Update it in Settings.".into());
    }
    Ok(())
}

#[cfg(test)]
mod legacy_oauth_conversion_tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn conversion_preserves_the_account_identity_and_credential_key() {
        let id = Uuid::new_v4();
        let mut account = Account {
            id,
            email: "legacy@gmail.com".into(),
            account_name: "Personal Gmail".into(),
            display_name: "Legacy User".into(),
            provider_id: "gmail".into(),
            auth: AccountAuth::OAuth2 {
                username: "legacy@gmail.com".into(),
                provider: "gmail".into(),
                access_token_expires_at: None,
            },
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::Security::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::Security::Tls,
            archive_mailbox: "[Gmail]/All Mail".into(),
            spam_mailbox: "[Gmail]/Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let previous_secret_name = credential_secret_name(&account);

        assert!(convert_legacy_oauth_to_password(&mut account));

        assert_eq!(account.id, id);
        assert_eq!(account.account_name, "Personal Gmail");
        assert_eq!(account.archive_mailbox, "[Gmail]/All Mail");
        assert_eq!(credential_secret_name(&account), previous_secret_name);
        assert!(matches!(account.auth, AccountAuth::Password { .. }));
    }

    #[test]
    fn password_accounts_are_not_converted() {
        let mut account = AccountDraft {
            email: "already-password@example.test".into(),
            display_name: "Password User".into(),
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

        assert!(!convert_legacy_oauth_to_password(&mut account));
        assert!(matches!(account.auth, AccountAuth::Password { .. }));
    }

    #[test]
    fn app_password_conversion_requires_a_paused_legacy_gmail_oauth_account() {
        let id = Uuid::new_v4();
        let mut gmail = Account {
            id,
            email: "legacy@gmail.com".into(),
            account_name: "Legacy Gmail".into(),
            display_name: "Legacy User".into(),
            provider_id: "gmail".into(),
            auth: AccountAuth::OAuth2 {
                username: "legacy@gmail.com".into(),
                provider: "gmail".into(),
                access_token_expires_at: None,
            },
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::Security::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::Security::Tls,
            archive_mailbox: "[Gmail]/All Mail".into(),
            spam_mailbox: "[Gmail]/Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let paused_authentication = vec![RealtimeSyncStatus {
            account_id: id,
            state: "paused".into(),
            retry_at: None,
            error_kind: Some("authentication".into()),
        }];

        assert!(validate_legacy_oauth_conversion(&gmail, &paused_authentication).is_ok());
        assert!(validate_legacy_oauth_conversion(&gmail, &[])
            .unwrap_err()
            .contains("after its existing sign-in stops working"));

        gmail.provider_id = "outlook".into();
        gmail.auth = AccountAuth::OAuth2 {
            username: "legacy@gmail.com".into(),
            provider: "outlook".into(),
            access_token_expires_at: None,
        };
        assert!(
            validate_legacy_oauth_conversion(&gmail, &paused_authentication)
                .unwrap_err()
                .contains("Only legacy Gmail OAuth accounts")
        );
    }

    #[test]
    fn add_account_rejects_case_insensitive_duplicate_email() {
        let existing = AccountDraft {
            email: "already-connected@example.test".into(),
            display_name: "Connected User".into(),
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

        assert!(
            ensure_account_is_not_connected(&[existing], " ALREADY-CONNECTED@example.test ")
                .unwrap_err()
                .contains("already connected")
        );
    }
}

async fn enabled_account_for_operation(
    state: &Arc<AppState>,
    account_id: Uuid,
) -> Result<Account, String> {
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    if !account.enabled {
        return Err("Account is disabled".to_owned());
    }
    Ok(account)
}

/// Search account IDs are untrusted request data. Keep local catalogue reads
/// aligned with provider work by intersecting an explicit selection with the
/// accounts that are currently enabled. This also prevents a disabled
/// account's cached mail from appearing in a mixed-account result page.
fn enabled_search_account_ids(accounts: &[Account], requested: &[Uuid]) -> Vec<Uuid> {
    if requested.is_empty() {
        return accounts
            .iter()
            .filter(|account| account.enabled)
            .map(|account| account.id)
            .collect();
    }

    let enabled = accounts
        .iter()
        .filter(|account| account.enabled)
        .map(|account| account.id)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    requested
        .iter()
        .copied()
        .filter(|account_id| enabled.contains(account_id) && seen.insert(*account_id))
        .collect()
}

fn explicit_search_scope_has_no_enabled_accounts(requested: &[Uuid], enabled: &[Uuid]) -> bool {
    !requested.is_empty() && enabled.is_empty()
}

/// The legacy search command predates V2 coverage responses, but it must not
/// let Storage's historical empty-account convention widen a disabled or
/// deleted explicit scope into every local account.
fn legacy_search_query_for_enabled_accounts(
    mut query: SearchQuery,
    accounts: &[Account],
) -> Option<SearchQuery> {
    let account_ids = enabled_search_account_ids(accounts, &query.account_ids);
    if account_ids.is_empty() {
        return None;
    }
    query.account_ids = account_ids;
    Some(query)
}

/// Account mutations invalidate affected hybrid searches before changing
/// configuration or local message state. Provider workers check the shared
/// session token before every result write, so a stale IMAP response cannot
/// publish after this boundary.
async fn invalidate_searches_for_account(state: &AppState, account_id: Uuid) -> Result<(), String> {
    state.search_sessions.cancel_account(account_id);
    state
        .store
        .advance_account_search_generation(account_id)
        .await
        .map_err(error)?;
    Ok(())
}

async fn restart_realtime_if_current(
    app: tauri::AppHandle,
    state: &Arc<AppState>,
    account_id: Uuid,
) -> anyhow::Result<()> {
    // Never use an account snapshot captured before provider work: it may
    // have been removed or disabled while that work was finishing.
    if let Some(account) = state.store.account(account_id).await? {
        if account.enabled {
            state.realtime.start_account(app, account).await;
        }
    }
    Ok(())
}

fn complete_manual_sync_attempt<T>(
    refresh: anyhow::Result<T>,
    restart: anyhow::Result<()>,
    account_id: Uuid,
) -> anyhow::Result<T> {
    match (refresh, restart) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(restart_error)) => Err(restart_error),
        (Err(refresh_error), Ok(())) => Err(refresh_error),
        (Err(refresh_error), Err(restart_error)) => {
            // The caller asked to sync. Preserve that failure while still
            // recording that recovery also could not start.
            tracing::warn!(
                account_id = %account_id,
                error = %restart_error,
                "could not restart real-time mail after a failed manual sync"
            );
            Err(refresh_error)
        }
    }
}

#[cfg(test)]
mod account_operation_lock_tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: Uuid::new_v4(),
            email: "reader@example.com".into(),
            account_name: "Reader".into(),
            display_name: "Reader".into(),
            provider_id: "fastmail".into(),
            auth: AccountAuth::Password {
                username: "reader@example.com".into(),
            },
            imap_host: "imap.fastmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::provider::Security::Tls,
            smtp_host: "smtp.fastmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::provider::Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn serializes_the_same_account_without_blocking_the_lock_registry() {
        let locks = Arc::new(AccountOperationLocks::default());
        let account_id = Uuid::new_v4();
        let first = locks.acquire(account_id).await;
        let waiting = locks.clone();
        let (acquired, mut receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _second = waiting.acquire(account_id).await;
            let _ = acquired.send(());
        });

        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut receiver)
            .await
            .expect("second operation should acquire after the first exits")
            .expect("operation task should report acquisition");
        task.await.expect("operation task should finish");
    }

    #[tokio::test]
    async fn provider_search_lane_leaves_a_connection_slot_for_opening_mail() {
        let all_remote_operations = Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY));
        let search_lane = Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY));
        let mut running_searches = Vec::new();
        for _ in 0..REMOTE_SEARCH_CONCURRENCY {
            let search = search_lane.clone().acquire_owned().await.unwrap();
            let remote = all_remote_operations.clone().acquire_owned().await.unwrap();
            running_searches.push((search, remote));
        }

        assert_eq!(all_remote_operations.available_permits(), 1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                all_remote_operations.clone().acquire_owned(),
            )
            .await
            .is_ok(),
            "an open may acquire the reserved shared provider slot while search pages run"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                search_lane.clone().acquire_owned()
            )
            .await
            .is_err(),
            "a fourth search waits in the dedicated search lane"
        );
        drop(running_searches);
    }

    #[tokio::test]
    async fn provider_search_does_not_wait_for_the_account_mutation_lock() {
        let locks = Arc::new(AccountOperationLocks::default());
        let account_id = Uuid::new_v4();
        let _sending = locks.acquire(account_id).await;
        let remote = Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY));
        let search = Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY));
        let (started, receiver) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            // This matches the provider-search scheduling path: it reserves
            // remote capacity but intentionally never acquires `locks`.
            let _search = search.acquire_owned().await.unwrap();
            let _remote = remote.acquire_owned().await.unwrap();
            let _ = started.send(());
        });

        tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("a search must begin while a same-account send owns the mutation lock")
            .expect("search task should report start");
    }

    #[test]
    fn failed_refresh_remains_the_reported_error_when_recovery_also_fails() {
        let error = complete_manual_sync_attempt::<()>(
            Err(anyhow::anyhow!("refresh failed")),
            Err(anyhow::anyhow!("restart failed")),
            Uuid::nil(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "refresh failed");
    }

    #[test]
    fn restart_error_is_reported_after_a_successful_refresh() {
        let error = complete_manual_sync_attempt::<()>(
            Ok(()),
            Err(anyhow::anyhow!("restart failed")),
            Uuid::nil(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "restart failed");
    }

    #[tokio::test]
    async fn cancellation_reaches_a_rebuild_registered_while_waiting_for_its_account_lock() {
        let cancellations = MailRebuildCancellations::default();
        let account_id = Uuid::new_v4();
        let locks = AccountOperationLocks::default();
        let held_lock = locks.acquire(account_id).await;

        let receiver = cancellations.register(account_id);
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);

        assert_eq!(
            cancellations.disposition(&receiver),
            MailRebuildCancellationDisposition::Retain
        );
        drop(held_lock);
        cancellations.clear(account_id);

        // An account update with no registered rebuild must not turn into a
        // stale cancellation for a later, unrelated full sync.
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);
        let next_attempt = cancellations.register(account_id);
        assert_eq!(
            cancellations.disposition(&next_attempt),
            MailRebuildCancellationDisposition::None
        );
    }

    #[test]
    fn cancellation_reaches_a_reserved_rebuild_before_its_task_registers() {
        let cancellations = MailRebuildCancellations::default();
        let account_id = Uuid::new_v4();

        cancellations.reserve(account_id);
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);
        let receiver = cancellations.register(account_id);

        assert_eq!(
            cancellations.disposition(&receiver),
            MailRebuildCancellationDisposition::Retain
        );
        cancellations.clear(account_id);
        cancellations.release_reservation(account_id);

        // A later unrelated reservation gets a fresh channel, rather than a
        // stale cancellation from a completed update.
        cancellations.reserve(account_id);
        let next = cancellations.register(account_id);
        assert_eq!(
            cancellations.disposition(&next),
            MailRebuildCancellationDisposition::None
        );
    }

    #[test]
    fn durable_reset_intent_cannot_be_downgraded_by_a_queued_worker() {
        assert!(effective_rebuild_reset(false, Some(true)));
        assert!(effective_rebuild_reset(true, Some(false)));
        assert!(!effective_rebuild_reset(false, Some(false)));
    }

    #[test]
    fn initial_reset_jobs_survive_missing_or_rejected_credentials() {
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("mail rebuild cancelled"),
            MailRebuildCancellationDisposition::Remove,
            true,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP authentication rejected: invalid credentials"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP authentication rejected: invalid credentials"),
            MailRebuildCancellationDisposition::None,
            true,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("credentials are not stored for this account"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("credentials are not stored for this account"),
            MailRebuildCancellationDisposition::None,
            true,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP connection closed during command"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
    }

    #[test]
    fn cancellation_disposition_retains_replacement_but_removes_deleted_account_jobs() {
        let transient = anyhow::anyhow!("IMAP connection closed during command");
        let permanent = anyhow::anyhow!("IMAP authentication rejected");
        assert!(should_retain_mail_rebuild_job(
            &permanent,
            MailRebuildCancellationDisposition::Retain,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &transient,
            MailRebuildCancellationDisposition::Replace,
            false,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &transient,
            MailRebuildCancellationDisposition::Remove,
            true,
        ));
    }

    #[test]
    fn normalizes_email_identity_for_account_reuse() {
        assert_eq!(
            normalized_account_email(" Existing@Example.Com "),
            "existing@example.com"
        );
    }

    #[test]
    fn reuses_an_index_only_for_the_same_remote_namespace() {
        let existing = account();
        let same_remote = existing.clone();
        assert!(same_mail_namespace(&existing, &same_remote));

        let mut different_provider = same_remote.clone();
        different_provider.provider_id = "outlook".into();
        assert!(!same_mail_namespace(&existing, &different_provider));

        let mut different_host = same_remote.clone();
        different_host.imap_host = "imap.other.example".into();
        assert!(!same_mail_namespace(&existing, &different_host));

        let mut different_username = same_remote.clone();
        different_username.auth = AccountAuth::Password {
            username: "other@example.com".into(),
        };
        assert!(!same_mail_namespace(&existing, &different_username));

        let mut different_mailbox = same_remote;
        different_mailbox.archive_mailbox = "All Mail".into();
        assert!(!same_mail_namespace(&existing, &different_mailbox));
    }
}
#[derive(Default)]
struct ClassificationScheduleState {
    running: bool,
    requested_generation: u64,
    completed_generation: u64,
    last_completed_count: usize,
    last_failure: Option<(u64, String)>,
}

#[derive(Default)]
struct ClassificationScheduler {
    state: Mutex<ClassificationScheduleState>,
    completed: Notify,
}

impl ClassificationScheduler {
    /// Coalesce repeated native kicks into one runner while retaining a
    /// generation for every request that arrives during a drain.
    fn request(&self) -> (u64, bool) {
        let mut state = self.state.lock().expect("classification lock poisoned");
        state.requested_generation += 1;
        let generation = state.requested_generation;
        let should_start = !state.running;
        if should_start {
            state.running = true;
        }
        (generation, should_start)
    }

    fn next_generation(&self) -> u64 {
        self.state
            .lock()
            .expect("classification lock poisoned")
            .requested_generation
    }

    /// Returns true when a kick arrived while this pass was running, so the
    /// current runner must make another database pass before it may stop.
    fn finish_generation(&self, generation: u64, classified: usize) -> bool {
        let mut state = self.state.lock().expect("classification lock poisoned");
        if state.requested_generation > generation {
            return true;
        }
        state.completed_generation = generation;
        state.last_completed_count = classified;
        state.last_failure = None;
        state.running = false;
        self.completed.notify_waiters();
        false
    }

    /// Returns true when a newer kick must be retried after a failed pass.
    fn fail_generation(&self, generation: u64, error: String) -> bool {
        let mut state = self.state.lock().expect("classification lock poisoned");
        state.completed_generation = generation;
        state.last_failure = Some((generation, error));
        let retry = state.requested_generation > generation;
        state.running = retry;
        self.completed.notify_waiters();
        retry
    }

    async fn wait_for(&self, generation: u64) -> anyhow::Result<usize> {
        loop {
            let notified = self.completed.notified();
            let completed = {
                let state = self.state.lock().expect("classification lock poisoned");
                if state.completed_generation < generation {
                    None
                } else if let Some((failed_generation, failure)) = &state.last_failure {
                    if *failed_generation >= generation {
                        Some(Err(anyhow::anyhow!(failure.clone())))
                    } else {
                        Some(Ok(state.last_completed_count))
                    }
                } else {
                    Some(Ok(state.last_completed_count))
                }
            };
            if let Some(result) = completed {
                return result;
            }
            notified.await;
        }
    }
}

fn classification_retry_delay(consecutive_failures: usize) -> Option<Duration> {
    CLASSIFICATION_RETRY_DELAYS
        .get(consecutive_failures.saturating_sub(1))
        .copied()
}

fn validate_classification_output_count(
    input_count: usize,
    output_count: usize,
) -> anyhow::Result<()> {
    if input_count != output_count {
        return Err(anyhow::anyhow!(
            "classifier returned {output_count} results for {input_count} messages"
        ));
    }
    Ok(())
}

fn validate_classification_apply_count(attempted: usize, applied: usize) -> anyhow::Result<usize> {
    if attempted != applied {
        anyhow::bail!(
            "classification batch became stale ({applied}/{attempted} results applied); retrying with current evidence"
        );
    }
    Ok(applied)
}

async fn retry_classification_batch<T, F, Fut>(mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut failures = 0;
    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(error) => {
                failures += 1;
                let Some(delay) = classification_retry_delay(failures) else {
                    return Err(error);
                };
                tracing::warn!(
                    attempt = failures,
                    retry_delay_ms = delay.as_millis(),
                    error = %error,
                    "classification batch failed; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod classification_scheduler_tests {
    use super::*;

    #[test]
    fn coalesces_kicks_without_losing_one_that_arrives_during_a_pass() {
        let scheduler = ClassificationScheduler::default();
        let (first, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let (second, starts_second_runner) = scheduler.request();
        assert!(!starts_second_runner);
        assert!(scheduler.finish_generation(first, 64));
        assert_eq!(scheduler.next_generation(), second);
        assert!(!scheduler.finish_generation(second, 65));
    }

    #[tokio::test]
    async fn manual_waiter_observes_the_serialized_drain_result() {
        let scheduler = Arc::new(ClassificationScheduler::default());
        let (generation, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.wait_for(generation).await })
        };
        tokio::task::yield_now().await;
        assert!(!scheduler.finish_generation(generation, 7));
        assert_eq!(waiter.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn failed_drain_wakes_waiters_and_a_later_kick_starts_a_new_drain() {
        let scheduler = Arc::new(ClassificationScheduler::default());
        let (failed_generation, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.wait_for(failed_generation).await })
        };
        tokio::task::yield_now().await;
        assert!(!scheduler.fail_generation(failed_generation, "classifier unavailable".into()));
        assert_eq!(
            waiter.await.unwrap().unwrap_err().to_string(),
            "classifier unavailable"
        );

        let (recovery_generation, starts_recovery) = scheduler.request();
        assert!(starts_recovery);
        assert!(!scheduler.finish_generation(recovery_generation, 4));
        assert_eq!(scheduler.wait_for(recovery_generation).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn retries_a_transient_batch_failure_without_an_external_kick() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let classified = retry_classification_batch({
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Err(anyhow::anyhow!("temporary classifier failure"))
                    } else {
                        Ok(6)
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(classified, 6);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_policy_has_two_bounded_backoff_delays() {
        assert_eq!(
            classification_retry_delay(1),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            classification_retry_delay(2),
            Some(Duration::from_millis(500))
        );
        assert_eq!(classification_retry_delay(3), None);
    }

    #[test]
    fn rejects_classifier_output_that_does_not_cover_the_entire_batch() {
        assert!(validate_classification_output_count(3, 3).is_ok());
        assert_eq!(
            validate_classification_output_count(3, 2)
                .unwrap_err()
                .to_string(),
            "classifier returned 2 results for 3 messages"
        );
        assert_eq!(
            validate_classification_output_count(3, 4)
                .unwrap_err()
                .to_string(),
            "classifier returned 4 results for 3 messages"
        );
    }

    #[test]
    fn stale_classification_apply_is_retryable_instead_of_counted_as_progress() {
        assert_eq!(validate_classification_apply_count(3, 3).unwrap(), 3);
        assert_eq!(
            validate_classification_apply_count(3, 0)
                .unwrap_err()
                .to_string(),
            "classification batch became stale (0/3 results applied); retrying with current evidence"
        );
    }
}

async fn run_bounded_ordered<T, U, F, Fut>(
    items: Vec<T>,
    max_in_flight: usize,
    limiter: Arc<Semaphore>,
    operation: F,
) -> Vec<U>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = U> + Send + 'static,
{
    assert!(max_in_flight > 0, "bounded work requires a non-zero limit");
    let mut pending = items.into_iter().enumerate();
    let mut active = tokio::task::JoinSet::new();
    let mut completed = Vec::new();

    loop {
        while active.len() < max_in_flight {
            let Some((index, item)) = pending.next() else {
                break;
            };
            let operation = operation.clone();
            let limiter = limiter.clone();
            active.spawn(async move {
                let _permit = limiter
                    .acquire_owned()
                    .await
                    .expect("shared operation limiter must remain open");
                (index, operation(item).await)
            });
        }
        let Some(joined) = active.join_next().await else {
            break;
        };
        completed.push(joined.expect("bounded task must not panic"));
    }

    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, output)| output).collect()
}

#[cfg(test)]
mod bounded_concurrency_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn caps_work_and_restores_input_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let outputs = run_bounded_ordered((0..12).collect(), 3, Arc::new(Semaphore::new(12)), {
            let active = active.clone();
            let peak = peak.clone();
            move |index| {
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis((12 - index) as u64)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    index
                }
            }
        })
        .await;

        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(outputs, (0..12).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn keeps_failures_in_input_order() {
        let outputs = run_bounded_ordered(
            (0..4).collect(),
            4,
            Arc::new(Semaphore::new(4)),
            |index| async move {
                tokio::time::sleep(Duration::from_millis((4 - index) as u64)).await;
                if matches!(index, 1 | 3) {
                    Err(index)
                } else {
                    Ok(index)
                }
            },
        )
        .await;

        assert_eq!(outputs, vec![Ok(0), Err(1), Ok(2), Err(3)]);
        assert_eq!(outputs.into_iter().find_map(Result::err), Some(1));
    }

    #[tokio::test]
    async fn dropping_bounded_work_aborts_in_flight_tasks() {
        struct ActiveGuard(Arc<AtomicUsize>);
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let limiter = Arc::new(Semaphore::new(2));
        let task = tokio::spawn(run_bounded_ordered(
            (0..20).collect(),
            2,
            limiter.clone(),
            {
                let active = active.clone();
                move |_| {
                    let active = active.clone();
                    async move {
                        active.fetch_add(1, Ordering::SeqCst);
                        let _guard = ActiveGuard(active);
                        std::future::pending::<()>().await
                    }
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the bounded batch should start");

        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(limiter.available_permits(), 2);
    }

    #[tokio::test]
    async fn overlapping_invocations_share_the_application_limit() {
        let limiter = Arc::new(Semaphore::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let invoke = |offset| {
            let limiter = limiter.clone();
            let active = active.clone();
            let peak = peak.clone();
            tokio::spawn(run_bounded_ordered(
                (0..8).map(|index| offset + index).collect(),
                4,
                limiter,
                move |index| {
                    let active = active.clone();
                    let peak = peak.clone();
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        index
                    }
                },
            ))
        };

        let (left, right) = tokio::join!(invoke(0), invoke(100));
        assert_eq!(left.unwrap().len(), 8);
        assert_eq!(right.unwrap().len(), 8);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(limiter.available_permits(), 3);
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailRebuildProgress {
    account_id: Uuid,
    phase: String,
    completed: usize,
    total: Option<usize>,
    #[serde(skip)]
    reset_before_sync: bool,
}

impl From<MailRebuildJob> for MailRebuildProgress {
    fn from(job: MailRebuildJob) -> Self {
        Self {
            account_id: job.account_id,
            phase: job.phase,
            completed: job.completed,
            total: job.total,
            reset_before_sync: job.reset_before_sync,
        }
    }
}

const MAX_DROPPED_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_DROPPED_ATTACHMENT_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_DROPPED_ATTACHMENTS: usize = 50;
const DROPPED_FILE_RECEIPT_TTL: Duration = Duration::from_secs(30);
const DROPPED_FILE_RECEIPT_EVENT: &str = "dakia://dropped-file-receipt";
const DROPPED_FILE_ERROR_EVENT: &str = "dakia://dropped-file-error";
#[cfg(target_os = "macos")]
const TERMINAL_COMMAND_PATH: &str = "/usr/local/bin/dakia";

struct DroppedFileReceipt {
    window_label: String,
    files: Vec<OpenDroppedFile>,
    expires_at: Instant,
}

#[derive(Default)]
struct DroppedFileReceiptStore {
    entries: Mutex<HashMap<String, DroppedFileReceipt>>,
}

impl DroppedFileReceiptStore {
    fn issue(&self, window_label: &str, paths: Vec<PathBuf>) -> Result<String, String> {
        self.issue_at(window_label, paths, Instant::now())
    }

    fn issue_at(
        &self,
        window_label: &str,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> Result<String, String> {
        if paths.is_empty() {
            return Err("No files were dropped".into());
        }
        let files = open_dropped_files(paths)?;
        let receipt = Uuid::new_v4().to_string();
        let mut entries = self.entries.lock().map_err(error)?;
        entries.retain(|_, entry| entry.expires_at > now);
        entries.insert(
            receipt.clone(),
            DroppedFileReceipt {
                window_label: window_label.to_owned(),
                files,
                expires_at: now + DROPPED_FILE_RECEIPT_TTL,
            },
        );
        Ok(receipt)
    }

    fn consume(&self, receipt: &str, window_label: &str) -> Result<Vec<OpenDroppedFile>, String> {
        self.consume_at(receipt, window_label, Instant::now())
    }

    fn consume_at(
        &self,
        receipt: &str,
        window_label: &str,
        now: Instant,
    ) -> Result<Vec<OpenDroppedFile>, String> {
        let invalid = || "Dropped-file authorization is invalid or expired".to_owned();
        let mut entries = self.entries.lock().map_err(error)?;
        entries.retain(|_, entry| entry.expires_at > now);
        let entry = entries.get(receipt).ok_or_else(invalid)?;
        if entry.window_label != window_label {
            return Err(invalid());
        }
        Ok(entries
            .remove(receipt)
            .expect("receipt existed while the store lock was held")
            .files)
    }

    fn revoke_window(&self, window_label: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, entry| entry.window_label != window_label);
        }
    }

    fn expire_at(&self, receipt: &str, now: Instant) {
        if let Ok(mut entries) = self.entries.lock() {
            if entries
                .get(receipt)
                .is_some_and(|entry| entry.expires_at <= now)
            {
                entries.remove(receipt);
            }
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct DroppedAttachment {
    filename: String,
    mime_type: String,
    content_base64: String,
    size_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum TerminalCommandStatus {
    Available,
    NotSetUp,
    Conflict,
}

#[cfg(target_os = "macos")]
fn bundled_cli_path(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let path = std::env::current_exe()
        .map_err(error)?
        .parent()
        .ok_or_else(|| "Dakia could not locate its application directory.".to_string())?
        .join("dakia");
    if path.starts_with("/Volumes") {
        return Err(
            "Move Dakia to your Applications folder before setting up the terminal command.".into(),
        );
    }
    if path.is_file() {
        Ok(path)
    } else {
        Err("The Dakia terminal command is missing from this app installation.".into())
    }
}

#[cfg(target_os = "macos")]
fn terminal_command_status_for(source: &Path) -> TerminalCommandStatus {
    let destination = Path::new(TERMINAL_COMMAND_PATH);
    match std::fs::symlink_metadata(destination) {
        Err(error) if error.kind() == ErrorKind::NotFound => TerminalCommandStatus::NotSetUp,
        Err(_) => TerminalCommandStatus::Conflict,
        Ok(metadata) if !metadata.file_type().is_symlink() => TerminalCommandStatus::Conflict,
        Ok(_) => match std::fs::read_link(destination) {
            Ok(target) if target == source => TerminalCommandStatus::Available,
            _ => TerminalCommandStatus::Conflict,
        },
    }
}

#[cfg(target_os = "macos")]
fn set_terminal_menu_label(app: &tauri::AppHandle, status: &TerminalCommandStatus) {
    let Some(menu) = app.menu() else {
        return;
    };
    let Some(item) = menu.get("terminal-command") else {
        return;
    };
    let Some(item) = item.as_menuitem() else {
        return;
    };
    let label = match status {
        TerminalCommandStatus::Available => "Remove Dakia Terminal Command…",
        _ => "Use Dakia from Terminal…",
    };
    let _ = item.set_text(label);
}

#[cfg(target_os = "macos")]
fn run_privileged_terminal_command(script: &str, source: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "set sourcePath to item 1 of argv",
            "-e",
            script,
            "-e",
            "end run",
            "--",
        ])
        .arg(source)
        .output()
        .map_err(error)?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.contains("User canceled") {
            Err("Setup was canceled.".into())
        } else {
            Err(format!(
                "macOS could not update the terminal command. {detail}"
            ))
        }
    }
}

#[tauri::command]
fn terminal_command_status(app: tauri::AppHandle) -> Result<TerminalCommandStatus, String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        Ok(terminal_command_status_for(&source))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[tauri::command]
async fn install_terminal_command(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        match terminal_command_status_for(&source) {
            TerminalCommandStatus::Available => return Ok(()),
            TerminalCommandStatus::Conflict => {
                return Err(format!(
                    "Another item already exists at {TERMINAL_COMMAND_PATH}. It was left unchanged."
                ))
            }
            TerminalCommandStatus::NotSetUp => {}
        }
        let install_source = source.clone();
        tauri::async_runtime::spawn_blocking(move || {
            run_privileged_terminal_command(
                "do shell script \"/bin/mkdir -p /usr/local/bin && /bin/test ! -e /usr/local/bin/dakia && /bin/test ! -L /usr/local/bin/dakia && /bin/ln -s \" & quoted form of sourcePath & \" /usr/local/bin/dakia\" with administrator privileges",
                &install_source,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        let status = terminal_command_status_for(&source);
        if !matches!(status, TerminalCommandStatus::Available) {
            return Err("The terminal command could not be verified after setup.".into());
        }
        set_terminal_menu_label(&app, &status);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[tauri::command]
async fn remove_terminal_command(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        match terminal_command_status_for(&source) {
            TerminalCommandStatus::NotSetUp => return Ok(()),
            TerminalCommandStatus::Conflict => {
                return Err(format!(
                    "{TERMINAL_COMMAND_PATH} does not belong to this copy of Dakia and was left unchanged."
                ))
            }
            TerminalCommandStatus::Available => {}
        }
        let remove_source = source.clone();
        tauri::async_runtime::spawn_blocking(move || {
            run_privileged_terminal_command(
                "do shell script \"current=$(/usr/bin/readlink /usr/local/bin/dakia 2>/dev/null); /bin/test \\\"$current\\\" = \" & quoted form of sourcePath & \" && /bin/rm /usr/local/bin/dakia\" with administrator privileges",
                &remove_source,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        let status = terminal_command_status_for(&source);
        if !matches!(status, TerminalCommandStatus::NotSetUp) {
            return Err("The terminal command could not be verified after removal.".into());
        }
        set_terminal_menu_label(&app, &status);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[derive(Serialize)]
struct MessageContent {
    body_text: String,
    body_html: Option<String>,
    unsubscribe_kind: Option<String>,
    attachments: Vec<Attachment>,
}

/// The reader must distinguish content that cannot change on another fetch
/// from a transient provider failure. Keep the IPC payload deliberately small:
/// parser and provider diagnostics are useful locally, but are not safe UI
/// text.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MessageContentErrorKind {
    ResourceLimit,
    Malformed,
    Undecodable,
    Unsupported,
    Transient,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct MessageContentCommandError {
    kind: MessageContentErrorKind,
}

impl MessageContentCommandError {
    fn from_failure(failure: &str) -> Self {
        let failure = failure.to_ascii_lowercase();
        let resource_limit = [
            "mime_raw_message_too_large",
            "mime_headers_too_large",
            "mime_too_many_parts",
            "mime_multipart_nesting_too_deep",
            "mime_resolved_html_too_large",
            "mime safety limit",
            "safety limit",
            "message has too many attachments",
            "message has more than",
            "message display body exceeds",
            "message display body size overflow",
            "attachment bytes overflowed",
            "mime part count overflow",
        ]
        .iter()
        .any(|marker| failure.contains(marker));
        let malformed = [
            "bodystructure",
            "mime part headers are malformed",
            "mime part parser omitted",
            "message parser could not find",
        ]
        .iter()
        .any(|marker| failure.contains(marker));
        let kind = if resource_limit {
            MessageContentErrorKind::ResourceLimit
        } else if failure.contains("mime_content_undecodable") {
            MessageContentErrorKind::Undecodable
        } else if failure.contains("unsupported transfer encoding") {
            MessageContentErrorKind::Unsupported
        } else if malformed {
            MessageContentErrorKind::Malformed
        } else {
            MessageContentErrorKind::Transient
        };
        Self { kind }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopNotification {
    title: String,
    body: String,
    account_id: Option<String>,
    message_id: Option<String>,
    thread_id: Option<String>,
    rfc_message_id: Option<String>,
    count: usize,
    sound: Option<String>,
}

#[cfg(any(target_os = "macos", test))]
fn notification_has_reader_target(notification: &DesktopNotification) -> bool {
    notification.count == 1
        && notification
            .account_id
            .as_deref()
            .is_some_and(|account_id| !account_id.trim().is_empty())
        && [
            notification.message_id.as_deref(),
            notification.rfc_message_id.as_deref(),
            notification.thread_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod desktop_notification_tests {
    use super::*;

    #[test]
    fn single_message_notifications_preserve_reader_locators_without_focusing_main() {
        let notification = DesktopNotification {
            title: "New message".into(),
            body: "A reply arrived".into(),
            account_id: Some("account-1".into()),
            message_id: Some("account-1:INBOX:7".into()),
            thread_id: Some("root@example.test".into()),
            rfc_message_id: Some("<reply@example.test>".into()),
            count: 1,
            sound: None,
        };
        assert!(notification_has_reader_target(&notification));
        let value = serde_json::to_value(notification).unwrap();
        assert_eq!(value["threadId"], "root@example.test");
        assert_eq!(value["rfcMessageId"], "<reply@example.test>");

        let grouped = DesktopNotification {
            title: "New messages".into(),
            body: "Several messages arrived".into(),
            account_id: Some("account-1".into()),
            message_id: Some("account-1:INBOX:7".into()),
            thread_id: Some("root@example.test".into()),
            rfc_message_id: Some("<reply@example.test>".into()),
            count: 2,
            sound: None,
        };
        assert!(!notification_has_reader_target(&grouped));
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddAccountInput {
    draft: AccountDraft,
    password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountConnection {
    account: Account,
    reused_existing_account: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAccountInput {
    id: Uuid,
    account_name: String,
    display_name: String,
    imap_host: String,
    imap_port: u16,
    imap_security: dakia_core::provider::Security,
    smtp_host: String,
    smtp_port: u16,
    smtp_security: dakia_core::provider::Security,
    archive_mailbox: String,
    spam_mailbox: String,
    password: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiInput {
    provider: String,
    base_url: Option<String>,
    model: String,
    api_key: Option<String>,
    executable: Option<PathBuf>,
    model_path: Option<PathBuf>,
    message_ids: Vec<String>,
    instruction: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UnsubscribeResult {
    Completed {
        #[serde(rename = "cleanupTarget")]
        cleanup_target: Option<SenderCleanupTarget>,
    },
    OpenedWeb {
        #[serde(rename = "cleanupTarget")]
        cleanup_target: Option<SenderCleanupTarget>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SenderCleanupTarget {
    account_id: Uuid,
    sender_name: Option<String>,
    sender_address: String,
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn sender_cleanup_target(message: &MailSummary, account_id: Uuid) -> Option<SenderCleanupTarget> {
    let sender_address = normalize_sender_address(&message.from_address)?;
    let sender_name = message
        .from_name
        .as_deref()
        .map(|name| {
            name.chars()
                .filter(|character| !character.is_control())
                .take(256)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|name| !name.is_empty());
    Some(SenderCleanupTarget {
        account_id,
        sender_name,
        sender_address,
    })
}

fn unsubscribe_email(
    account_id: Uuid,
    to: String,
    subject: String,
    body: String,
) -> Result<ComposeMessage, String> {
    if to.is_empty()
        || to.len() > 320
        || to.contains(['\r', '\n', '\0'])
        || subject.len() > 998
        || subject.contains(['\r', '\n', '\0'])
        || body.len() > 64 * 1024
        || body.contains('\0')
    {
        return Err("This message has an invalid unsubscribe email request".into());
    }
    Ok(ComposeMessage {
        account_id,
        to: vec![to],
        cc: vec![],
        bcc: vec![],
        subject,
        body_text: body,
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
    })
}

#[cfg(test)]
mod unsubscribe_email_tests {
    use super::*;

    #[test]
    fn builds_a_plain_single_recipient_message() {
        let draft = unsubscribe_email(
            Uuid::nil(),
            "token@unsubscribe.example".into(),
            "unsubscribe".into(),
            "Please unsubscribe me".into(),
        )
        .expect("valid unsubscribe email");

        assert_eq!(draft.to, ["token@unsubscribe.example"]);
        assert!(draft.cc.is_empty());
        assert!(draft.bcc.is_empty());
        assert_eq!(draft.subject, "unsubscribe");
        assert_eq!(draft.body_text, "Please unsubscribe me");
        assert!(draft.body_html.is_none());
        assert!(draft.attachments.is_empty());
    }

    #[test]
    fn rejects_recipient_and_subject_header_injection() {
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com\r\nBcc: victim@example.com".into(),
            String::new(),
            String::new(),
        )
        .is_err());
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com".into(),
            "unsubscribe\r\nBcc: victim@example.com".into(),
            String::new(),
        )
        .is_err());
    }

    #[test]
    fn rejects_oversized_mailto_content() {
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com".into(),
            String::new(),
            "x".repeat(64 * 1024 + 1),
        )
        .is_err());
    }

    #[test]
    fn unsubscribe_result_exposes_an_immutable_sender_cleanup_target() {
        let result = UnsubscribeResult::OpenedWeb {
            cleanup_target: Some(SenderCleanupTarget {
                account_id: Uuid::nil(),
                sender_name: Some("Newsletter".into()),
                sender_address: "news@example.test".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "kind": "opened_web",
                "cleanupTarget": {
                    "accountId": Uuid::nil(),
                    "senderName": "Newsletter",
                    "senderAddress": "news@example.test"
                }
            })
        );
    }

    #[test]
    fn malformed_sender_does_not_block_the_unsubscribe_result() {
        assert!(normalize_sender_address("not a mailbox").is_none());
        assert_eq!(
            serde_json::to_value(UnsubscribeResult::Completed {
                cleanup_target: None,
            })
            .unwrap(),
            serde_json::json!({ "kind": "completed", "cleanupTarget": null })
        );
    }
}

#[tauri::command]
async fn message_attachments(
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<Vec<Attachment>, String> {
    Ok(load_message_content(state.inner(), &message_id)
        .await?
        .attachments)
}

#[tauri::command]
async fn message_content(
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<MessageContent, MessageContentCommandError> {
    load_message_content(state.inner(), &message_id)
        .await
        .map_err(|failure| MessageContentCommandError::from_failure(&failure))
}

async fn cached_message_content(
    store: &Store,
    message_id: &str,
) -> Result<Option<MessageContent>, String> {
    if let Some((body_text, body_html)) = store.starred_body(message_id).await.map_err(error)? {
        return Ok(Some(MessageContent {
            body_text,
            body_html,
            unsubscribe_kind: store
                .message(message_id)
                .await
                .map_err(error)?
                .and_then(|message| message.unsubscribe_kind),
            attachments: store
                .starred_attachment_metadata(message_id)
                .await
                .map_err(error)?
                .into_iter()
                .filter(is_downloadable_attachment)
                .collect(),
        }));
    }
    if let Some(cached) = store
        .cached_message_content(message_id)
        .await
        .map_err(error)?
    {
        return Ok(Some(MessageContent {
            body_text: cached.body_text,
            body_html: cached.body_html,
            unsubscribe_kind: cached.unsubscribe_kind,
            attachments: cached
                .attachments
                .into_iter()
                .filter(is_downloadable_attachment)
                .collect(),
        }));
    }
    Ok(None)
}

async fn load_message_content(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<MessageContent, String> {
    if let Some(cached) = cached_message_content(&state.store, message_id).await? {
        if !looks_like_misclassified_text_body(&cached) {
            return Ok(cached);
        }
    }

    // Background warming and a foreground open share this durable claim. A
    // foreground request waits for the warmer's cache commit, but takes over
    // immediately if the warmer failed and released the claim.
    let mut waited = Duration::ZERO;
    let claim = loop {
        match state
            .store
            .acquire_message_content_fetch_outcome(message_id)
            .await
            .map_err(error)?
        {
            MessageContentFetchAcquire::Claimed(claim) => break claim,
            MessageContentFetchAcquire::Missing => return Err("Message not found".to_owned()),
            MessageContentFetchAcquire::Busy => {}
        }
        if let Some(cached) = cached_message_content(&state.store, message_id).await? {
            if !looks_like_misclassified_text_body(&cached) {
                return Ok(cached);
            }
        }
        if waited >= Duration::from_secs(60) {
            return Err("Timed out waiting for message content".to_owned());
        }
        let delay = Duration::from_millis(50);
        tokio::time::sleep(delay).await;
        waited += delay;
    };

    let result = async {
        // The winner must re-check after claiming: another fetch can commit
        // content immediately before releasing its claim.
        let cached_before_fetch = cached_message_content(&state.store, message_id).await?;
        if let Some(cached) = &cached_before_fetch {
            if !looks_like_misclassified_text_body(cached) {
                return Ok(MessageContent {
                    body_text: cached.body_text.clone(),
                    body_html: cached.body_html.clone(),
                    unsubscribe_kind: cached.unsubscribe_kind.clone(),
                    attachments: cached.attachments.clone(),
                });
            }
        }
        let message = if cached_before_fetch
            .as_ref()
            .is_some_and(looks_like_misclassified_text_body)
        {
            // PR #42 repairs legacy rows under the account-operation lock so
            // a concurrent move or action cannot redirect the refetch.
            refetch_and_persist_message(state, message_id).await?
        } else {
            fetch_remote_message(state, message_id).await?
        };
        let cached = CachedMessageContent {
            body_text: message.body_text.clone(),
            body_html: message.body_html.clone(),
            unsubscribe_kind: message.unsubscribe_kind.clone(),
            attachments: message
                .attachments
                .iter()
                .filter(|item| is_downloadable_attachment(&item.attachment))
                .map(|item| item.attachment.clone())
                .collect(),
        };
        let still_starred =
            persist_foreground_message_content(&state.store, &message, &cached).await?;
        if !still_starred {
            if let Err(cache_error) = state
                .store
                .cache_message_content(message_id, false, cached.clone())
                .await
            {
                tracing::warn!(%cache_error, %message_id, "could not persist foreground message cache");
            }
        }
        state
            .store
            .set_message_content_state(message_id, "complete")
            .await
            .map_err(error)?;
        Ok(MessageContent {
            body_text: cached.body_text,
            body_html: cached.body_html,
            unsubscribe_kind: cached.unsubscribe_kind,
            attachments: cached.attachments,
        })
    }
    .await;
    if let Err(release_error) = claim.release().await {
        tracing::warn!(%release_error, %message_id, "could not release message-content fetch claim");
    }
    result
}

fn looks_like_misclassified_text_body(content: &MessageContent) -> bool {
    content.body_text.trim().is_empty()
        && content
            .body_html
            .as_deref()
            .is_none_or(|html| html.trim().is_empty())
        && content.attachments.iter().any(|attachment| {
            attachment.is_inline
                && attachment.filename == "attachment"
                && matches!(attachment.mime_type.as_str(), "text/plain" | "text/html")
        })
}

#[cfg(test)]
mod message_content_repair_tests {
    use super::*;
    use dakia_core::AttachmentPresentation;

    struct UnexpectedClassifier;

    impl EmailClassifier for UnexpectedClassifier {
        fn classify(
            &mut self,
            _emails: &[EmailClassificationInput],
        ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
            panic!("message-content loading must not invoke email classification")
        }
    }

    fn message_content_test_state(store: Store) -> Arc<AppState> {
        Arc::new(AppState {
            realtime: RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: PathBuf::new(),
            classifier: Mutex::new(Box::new(UnexpectedClassifier)),
            classification_owner: "message-content-test".into(),
            classification: Arc::new(ClassificationScheduler::default()),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: MailRebuildCancellations::default(),
            account_operations: AccountOperationLocks::default(),
            search_sessions: SearchSessionRegistry::default(),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            remote_search_slots: Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY)),
            translation_downloads: Mutex::new(HashMap::new()),
            contacted_people_migration_drain: Arc::new(AsyncMutex::new(())),
        })
    }

    fn attachment(filename: &str, mime_type: &str, is_inline: bool) -> Attachment {
        Attachment {
            id: "message-1:0".into(),
            message_id: "message-1".into(),
            filename: filename.into(),
            mime_type: mime_type.into(),
            size_bytes: 42,
            is_inline,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: false,
        }
    }

    #[test]
    fn refetches_empty_content_with_phantom_inline_text_attachments() {
        let content = MessageContent {
            body_text: String::new(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: vec![
                attachment("attachment", "text/plain", true),
                attachment("attachment", "text/html", true),
            ],
        };

        assert!(looks_like_misclassified_text_body(&content));
    }

    #[test]
    fn preserves_legitimate_empty_messages_and_named_text_attachments() {
        let empty = MessageContent {
            body_text: String::new(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: Vec::new(),
        };
        let named_attachment = MessageContent {
            attachments: vec![attachment("notes.txt", "text/plain", true)],
            ..empty
        };

        assert!(!looks_like_misclassified_text_body(&named_attachment));
    }

    #[tokio::test]
    async fn deleted_message_content_fails_without_waiting_for_a_fetch_claim() {
        let state = message_content_test_state(Store::in_memory().await.expect("test store"));

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            load_message_content(&state, "deleted-message"),
        )
        .await
        .expect("a deleted message must not wait for the claim timeout");

        assert!(matches!(result, Err(error) if error == "Message not found"));
    }

    #[test]
    fn message_content_error_envelope_classifies_mime_failures_without_details() {
        let cases = [
            ("mime_raw_message_too_large", "resource_limit"),
            ("mime_content_undecodable", "undecodable"),
            (
                "MIME part uses an unsupported transfer encoding",
                "unsupported",
            ),
            ("BODYSTRUCTURE part is not a list", "malformed"),
            (
                "message display body exceeds the 50 MiB safety limit",
                "resource_limit",
            ),
            (
                "message display part exceeds the 25 MiB safety limit",
                "resource_limit",
            ),
            (
                "message parser could not find RFC 5322 headers",
                "malformed",
            ),
            ("IMAP connection reset by peer", "transient"),
        ];

        for (failure, kind) in cases {
            let serialized =
                serde_json::to_value(MessageContentCommandError::from_failure(failure))
                    .expect("message-content error must serialize for Tauri IPC");
            assert_eq!(serialized, serde_json::json!({ "kind": kind }));
            assert!(!serialized.to_string().contains(failure));
        }
    }
}

#[tauri::command]
async fn save_attachment(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    attachment_id: String,
) -> Result<String, String> {
    let (summary, account) = remote_message_locator(state.inner(), &message_id).await?;
    let attachment = MailService::new(state.store.clone())
        .fetch_attachment(
            &account,
            &summary.mailbox,
            summary.uid as u32,
            &attachment_id,
        )
        .await
        .map_err(error)?;
    save_to_downloads(&app, &attachment.attachment, &attachment.bytes).map_err(error)
}

#[tauri::command]
async fn export_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<String, String> {
    let initial_message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&initial_message.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message changed while it was being exported".to_owned())?;
    if !same_export_identity(&initial_message, &message) {
        return Err("Message changed while it was being exported".to_owned());
    }
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    let uid = u32::try_from(message.uid).map_err(|_| "Message UID is invalid".to_owned())?;
    let bytes = MailService::new(state.store.clone())
        .fetch_raw_message(&account, &message.mailbox, uid)
        .await
        .map_err(error)?;
    save_eml_to_downloads(&app, &message.subject, &bytes).map_err(error)
}

fn same_export_identity(before: &MailSummary, after: &MailSummary) -> bool {
    before.account_id == after.account_id
        && before.mailbox == after.mailbox
        && before.uid == after.uid
        && before.message_id == after.message_id
        && before.received_at == after.received_at
}

#[tauri::command]
async fn save_all_attachments(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<Vec<String>, String> {
    let attachments = fetch_full_remote_message(state.inner(), &message_id)
        .await?
        .attachments
        .into_iter()
        .filter(|item| is_downloadable_attachment(&item.attachment))
        .collect::<Vec<_>>();
    let mut saved = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        saved.push(
            save_to_downloads(&app, &attachment.attachment, &attachment.bytes).map_err(error)?,
        );
    }
    Ok(saved)
}

#[tauri::command]
async fn forward_attachments(
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<Vec<DroppedAttachment>, String> {
    let attachments = fetch_full_remote_message(state.inner(), &message_id)
        .await?
        .attachments
        .into_iter()
        .filter(|item| is_downloadable_attachment(&item.attachment))
        .collect::<Vec<_>>();
    if attachments.len() > MAX_DROPPED_ATTACHMENTS {
        return Err(format!(
            "This message has more than {MAX_DROPPED_ATTACHMENTS} attachments"
        ));
    }
    let total_bytes = attachments
        .iter()
        .map(|attachment| attachment.bytes.len() as u64)
        .sum::<u64>();
    if attachments
        .iter()
        .any(|attachment| attachment.bytes.len() as u64 > MAX_DROPPED_ATTACHMENT_BYTES)
        || total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES
    {
        return Err("The original attachments exceed the forwarding limit".into());
    }
    Ok(attachments
        .into_iter()
        .map(|attachment| DroppedAttachment {
            filename: attachment.attachment.filename,
            mime_type: attachment.attachment.mime_type,
            size_bytes: attachment.bytes.len() as u64,
            content_base64: STANDARD.encode(attachment.bytes),
        })
        .collect())
}

fn is_downloadable_attachment(attachment: &Attachment) -> bool {
    attachment.presentation.is_downloadable()
}

/// Authoritative foreground parsing corrects paperclip state for every
/// message. Starred content is written only when the current local row remains
/// starred; a fetched provider snapshot must never undo a concurrent unstar.
async fn persist_foreground_message_content(
    store: &Store,
    message: &MailSummary,
    content: &CachedMessageContent,
) -> Result<bool, String> {
    let exists = store
        .update_message_attachment_state(&message.id, message.has_attachments)
        .await
        .map_err(error)?;
    if !exists {
        return Ok(false);
    }
    if !message.is_flagged {
        return Ok(false);
    }
    store
        .cache_starred_message_content(&message.id, content.clone())
        .await
        .map_err(error)
}

#[cfg(test)]
mod attachment_presentation_command_tests {
    use super::*;
    use chrono::Utc;
    use dakia_core::{storage::AttachmentData, AttachmentPresentation};
    use tempfile::tempdir;

    struct NoopClassifier;

    impl EmailClassifier for NoopClassifier {
        fn classify(
            &mut self,
            _emails: &[EmailClassificationInput],
        ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
            Ok(Vec::new())
        }
    }

    fn operation_priority_test_state(store: Store) -> Arc<AppState> {
        Arc::new(AppState {
            realtime: RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: PathBuf::new(),
            classifier: Mutex::new(Box::new(NoopClassifier)),
            classification_owner: "operation-priority-test".into(),
            classification: Arc::new(ClassificationScheduler::default()),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: MailRebuildCancellations::default(),
            account_operations: AccountOperationLocks::default(),
            search_sessions: SearchSessionRegistry::default(),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            remote_search_slots: Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY)),
            translation_downloads: Mutex::new(HashMap::new()),
            contacted_people_migration_drain: Arc::new(AsyncMutex::new(())),
        })
    }

    fn attachment(id: &str, presentation: AttachmentPresentation) -> Attachment {
        Attachment {
            id: id.into(),
            message_id: "message-attachment-presentation".into(),
            filename: format!("{id}.bin"),
            mime_type: "application/octet-stream".into(),
            size_bytes: 3,
            is_inline: matches!(presentation, AttachmentPresentation::Embedded),
            presentation,
            is_potentially_unsafe: false,
        }
    }

    fn complete_message(account_id: String, is_flagged: bool) -> MailSummary {
        MailSummary {
            id: "message-attachment-presentation".into(),
            account_id,
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: Some("<attachment-presentation@example.test>".into()),
            in_reply_to: None,
            reference_ids: None,
            thread_id: "message-attachment-presentation".into(),
            subject: "Attachment presentation".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: "authoritative body".into(),
            body_text: "authoritative body".into(),
            body_html: Some("<p>authoritative body</p>".into()),
            content_state: "complete".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: true,
            is_flagged,
            is_answered: false,
            is_draft: false,
            has_attachments: true,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: vec![
                AttachmentData {
                    attachment: attachment("signature-logo", AttachmentPresentation::Embedded),
                    bytes: b"logo".to_vec(),
                },
                AttachmentData {
                    attachment: attachment("claim", AttachmentPresentation::Downloadable),
                    bytes: b"pdf".to_vec(),
                },
            ],
        }
    }

    fn cached_content(message: &MailSummary) -> CachedMessageContent {
        CachedMessageContent {
            body_text: message.body_text.clone(),
            body_html: message.body_html.clone(),
            unsubscribe_kind: message.unsubscribe_kind.clone(),
            attachments: message
                .attachments
                .iter()
                .filter(|item| is_downloadable_attachment(&item.attachment))
                .map(|item| item.attachment.clone())
                .collect(),
        }
    }

    async fn save_test_account(store: &Store) -> Account {
        let account = AccountDraft {
            email: "attachment-presentation@example.test".into(),
            display_name: "Attachment presentation".into(),
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
        .into_account(provider::by_id("fastmail").expect("Fastmail preset"));
        store.save_account(&account).await.expect("save account");
        account
    }

    #[test]
    fn command_boundary_exposes_only_downloadable_presentations() {
        let visible = [
            attachment("legacy", AttachmentPresentation::Unknown),
            attachment("signature", AttachmentPresentation::Embedded),
            attachment("document", AttachmentPresentation::Downloadable),
            attachment("explicit-cid", AttachmentPresentation::Both),
        ]
        .into_iter()
        .filter(is_downloadable_attachment)
        .map(|attachment| attachment.id)
        .collect::<Vec<_>>();

        assert_eq!(visible, ["document", "explicit-cid"]);
    }

    #[tokio::test]
    async fn authoritative_flagged_foreground_fetch_restores_starred_body_and_real_metadata() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = save_test_account(&store).await;

        // This is the post-migration state: the flagged catalogue message
        // survives, but its old starred body/attachment metadata was cleared.
        let catalogue = complete_message(account.id.to_string(), false);
        let message_id = catalogue.id.clone();
        store
            .upsert_messages(&[catalogue])
            .await
            .expect("save catalogue message");
        store
            .set_message_flagged(&message_id, true)
            .await
            .expect("flag message without a durable body");
        assert!(store
            .starred_body(&message_id)
            .await
            .expect("read starred body")
            .is_none());

        let fetched = complete_message(account.id.to_string(), true);
        assert!(
            persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist authoritative foreground fetch")
        );

        assert_eq!(
            store
                .starred_body(&message_id)
                .await
                .expect("read restored starred body")
                .expect("restored body")
                .0,
            "authoritative body"
        );
        let metadata = store
            .starred_attachment_metadata(&message_id)
            .await
            .expect("read restored attachment metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].id, "claim");
        assert_eq!(
            metadata[0].presentation,
            AttachmentPresentation::Downloadable
        );
    }

    #[tokio::test]
    async fn ordinary_foreground_fetch_persists_named_inline_paperclip_across_restart() {
        let directory = tempdir().expect("temporary store directory");
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.expect("open store");
        let account = save_test_account(&store).await;
        let mut catalogue = complete_message(account.id.to_string(), false);
        let message_id = catalogue.id.clone();
        // Header-only catalogue parsing cannot know whether an inline named
        // resource is used by the selected HTML branch.
        catalogue.has_attachments = false;
        catalogue.attachments.clear();
        store
            .upsert_messages(&[catalogue])
            .await
            .expect("save header-only catalogue message");

        let mut fetched = complete_message(account.id.to_string(), false);
        fetched.attachments[1].attachment.is_inline = true;
        assert!(
            !persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist ordinary foreground metadata")
        );
        assert!(
            store
                .message(&message_id)
                .await
                .expect("read collapsed message")
                .expect("message remains")
                .has_attachments
        );

        drop(store);
        let reopened = Store::open(&database).await.expect("reopen store");
        assert!(
            reopened
                .message(&message_id)
                .await
                .expect("read restarted message")
                .expect("message survives restart")
                .has_attachments
        );
    }

    #[tokio::test]
    async fn foreground_fetch_cannot_restore_a_message_unstarred_while_it_was_in_flight() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = save_test_account(&store).await;
        let fetched = complete_message(account.id.to_string(), true);
        let message_id = fetched.id.clone();
        store
            .upsert_messages(std::slice::from_ref(&fetched))
            .await
            .expect("save initially starred catalogue message");

        // The remote fetch observed `\\Flagged`, then the user unstarred the
        // current local row before its response could be persisted.
        store
            .set_message_flagged(&message_id, false)
            .await
            .expect("unstar during fetch");
        assert!(
            !persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist stale fetch without resurrecting the star")
        );

        assert!(
            !store
                .message(&message_id)
                .await
                .expect("read current message")
                .expect("message remains")
                .is_flagged
        );
        assert!(store
            .starred_body(&message_id)
            .await
            .expect("read starred cache")
            .is_none());
        assert!(store
            .starred_attachment_metadata(&message_id)
            .await
            .expect("read starred metadata")
            .is_empty());
    }

    #[tokio::test]
    async fn foreground_open_keeps_its_reserved_slot_during_provider_search_and_backfill_load() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted provider");
        let provider_port = listener.local_addr().expect("provider address").port();
        let store = Store::in_memory().await.expect("in-memory store");
        let mut account = save_test_account(&store).await;
        account.imap_host = "127.0.0.1".into();
        account.imap_port = provider_port;
        account.imap_security = dakia_core::provider::Security::Tls;
        store
            .save_account(&account)
            .await
            .expect("save scripted provider endpoint");
        MailService::new(store.clone())
            .credentials()
            .set_password(&account, "search secret")
            .await
            .expect("save scripted provider credential");

        let foreground = complete_message(account.id.to_string(), false);
        let foreground_id = foreground.id.clone();
        store
            .upsert_messages(std::slice::from_ref(&foreground))
            .await
            .expect("save foreground message");
        store
            .cache_message_content(&foreground_id, false, cached_content(&foreground))
            .await
            .expect("cache foreground message");

        // Leave enough real Sent work for the contacted-people backfill to
        // remain active while the foreground read runs.
        let mut sent = Vec::new();
        for uid in 1..=256_i64 {
            let mut message = complete_message(account.id.to_string(), false);
            message.id = format!("priority-sent-{uid}");
            message.thread_id = message.id.clone();
            message.message_id = Some(format!("<priority-sent-{uid}@example.test>"));
            message.mailbox = "Sent".into();
            message.uid = uid + 1;
            message.to_addresses = format!("Person {uid} <person{uid}@example.test>");
            message.attachments.clear();
            message.has_attachments = false;
            sent.push(message);
        }
        store
            .upsert_messages(&sent)
            .await
            .expect("save Sent backfill fixture");

        let state = operation_priority_test_state(store.clone());
        let (accepted_sender, mut accepted) = tokio::sync::mpsc::channel(REMOTE_SEARCH_CONCURRENCY);
        let provider = tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..REMOTE_SEARCH_CONCURRENCY {
                let (stream, _) = listener.accept().await.expect("accept provider search");
                connections.push(stream);
                accepted_sender
                    .send(())
                    .await
                    .expect("report provider connection");
            }
            std::future::pending::<()>().await;
            connections
        });

        let mut searches = Vec::new();
        for _ in 0..REMOTE_SEARCH_CONCURRENCY {
            let state = state.clone();
            let account = account.clone();
            searches.push(tokio::spawn(async move {
                let _search = state
                    .remote_search_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("search lane remains open");
                let _remote = state
                    .remote_operation_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("remote lane remains open");
                MailService::new(state.store.clone())
                    .search_remote(&account, "subject:needle", Some("INBOX"), 8)
                    .await
            }));
        }
        for _ in 0..REMOTE_SEARCH_CONCURRENCY {
            tokio::time::timeout(Duration::from_secs(2), accepted.recv())
                .await
                .expect("provider search must connect")
                .expect("provider connection report");
        }
        assert_eq!(
            state.remote_operation_slots.available_permits(),
            1,
            "provider search must leave one operation slot for foreground work"
        );

        let (first_batch_sender, first_batch) = tokio::sync::oneshot::channel();
        let (stop_sender, mut stop) = tokio::sync::watch::channel(false);
        let backfill_finished = Arc::new(AtomicBool::new(false));
        let background_finished = backfill_finished.clone();
        let backfill_store = store.clone();
        let account_id = account.id;
        let owner_address = account.email.clone();
        let backfill = tokio::spawn(async move {
            let mut first_batch_sender = Some(first_batch_sender);
            loop {
                let progress = backfill_store
                    .backfill_contacted_people_from_sent(
                        account_id,
                        std::slice::from_ref(&owner_address),
                        1,
                    )
                    .await
                    .expect("advance people backfill");
                if let Some(sender) = first_batch_sender.take() {
                    let _ = sender.send(());
                }
                if progress.complete {
                    background_finished.store(true, Ordering::SeqCst);
                    break;
                }
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => {},
                    _ = tokio::task::yield_now() => {},
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(2), first_batch)
            .await
            .expect("people backfill must start")
            .expect("people backfill start report");
        assert!(
            !backfill_finished.load(Ordering::SeqCst),
            "people backfill must still have queued work when foreground opening starts"
        );

        let started = Instant::now();
        let opened = tokio::time::timeout(
            Duration::from_secs(2),
            hydrate_messages(&state, std::slice::from_ref(&foreground_id)),
        )
        .await
        .expect("foreground open must not wait behind provider searches")
        .expect("foreground open succeeds from its complete local cache");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].body_text, "authoritative body");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "foreground open exceeded its conservative priority bound"
        );

        let _ = stop_sender.send(true);
        backfill.await.expect("people backfill exits cleanly");
        for search in searches {
            search.abort();
            let _ = search.await;
        }
        provider.abort();
        let _ = provider.await;
    }
}

async fn fetch_remote_message(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<MailSummary, String> {
    let (summary, account) = remote_message_locator(state, message_id).await?;
    MailService::new(state.store.clone())
        .fetch_message(&account, &summary.mailbox, summary.uid as u32)
        .await
        .map_err(error)
}

async fn fetch_full_remote_message(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<MailSummary, String> {
    let (summary, account) = remote_message_locator(state, message_id).await?;
    MailService::new(state.store.clone())
        .fetch_full_message(&account, &summary.mailbox, summary.uid as u32)
        .await
        .map_err(error)
}

async fn remote_message_locator(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<(MailSummary, Account), String> {
    let summary = state
        .store
        .messages_by_ids(&[message_id.to_owned()])
        .await
        .map_err(error)?
        .into_iter()
        .next()
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&summary.account_id).map_err(error)?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    Ok((summary, account))
}

async fn refetch_and_persist_message(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<MailSummary, String> {
    let initial_summary = state
        .store
        .messages_by_ids(&[message_id.to_owned()])
        .await
        .map_err(error)?
        .into_iter()
        .next()
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&initial_summary.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let summary = state
        .store
        .messages_by_ids(&[message_id.to_owned()])
        .await
        .map_err(error)?
        .into_iter()
        .next()
        .ok_or_else(|| "Message changed while it was being repaired".to_owned())?;
    if summary.account_id != initial_summary.account_id {
        return Err("Message changed while it was being repaired".to_owned());
    }
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    let message = MailService::new(state.store.clone())
        .fetch_message(&account, &summary.mailbox, summary.uid as u32)
        .await
        .map_err(error)?;
    let content = CachedMessageContent {
        body_text: message.body_text.clone(),
        body_html: message.body_html.clone(),
        unsubscribe_kind: message.unsubscribe_kind.clone(),
        attachments: message
            .attachments
            .iter()
            .map(|item| item.attachment.clone())
            .collect(),
    };
    persist_foreground_message_content(&state.store, &message, &content).await?;
    Ok(message)
}

struct OpenDroppedFile {
    filename: String,
    file: std::fs::File,
    metadata_len: u64,
}

fn open_dropped_file(path: PathBuf, index: usize) -> Result<OpenDroppedFile, String> {
    let filename = dakia_core::mail::safe_attachment_filename(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment"),
        index,
    );
    let link_metadata = std::fs::symlink_metadata(&path).map_err(error)?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err("Only regular files can be attached".into());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(&path).map_err(|open_error| {
        #[cfg(unix)]
        if open_error.raw_os_error() == Some(libc::ELOOP) {
            "Only regular files can be attached".to_owned()
        } else {
            error(open_error)
        }
        #[cfg(not(unix))]
        {
            error(open_error)
        }
    })?;
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() {
        return Err("Only regular files can be attached".into());
    }
    Ok(OpenDroppedFile {
        filename,
        file,
        metadata_len: metadata.len(),
    })
}

fn open_dropped_files(paths: Vec<PathBuf>) -> Result<Vec<OpenDroppedFile>, String> {
    if paths.len() > MAX_DROPPED_ATTACHMENTS {
        return Err(format!(
            "A message can include at most {MAX_DROPPED_ATTACHMENTS} attachments"
        ));
    }

    let mut total_bytes = 0_u64;
    let mut files = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        let file = open_dropped_file(path, index)?;
        if file.metadata_len > MAX_DROPPED_ATTACHMENT_BYTES {
            return Err(format!(
                "{} exceeds the {} MiB attachment limit",
                file.filename,
                MAX_DROPPED_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        total_bytes += file.metadata_len;
        if total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES {
            return Err(format!(
                "Attachments exceed the {} MiB total limit",
                MAX_DROPPED_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
            ));
        }
        files.push(file);
    }
    Ok(files)
}

fn materialize_dropped_files(
    files: Vec<OpenDroppedFile>,
) -> Result<Vec<DroppedAttachment>, String> {
    let mut total_bytes = 0;
    let mut attachments = Vec::with_capacity(files.len());
    for mut dropped_file in files {
        let mut bytes = Vec::with_capacity(dropped_file.metadata_len as usize);
        Read::by_ref(&mut dropped_file.file)
            .take(MAX_DROPPED_ATTACHMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() as u64 > MAX_DROPPED_ATTACHMENT_BYTES {
            return Err(format!(
                "{} exceeds the {} MiB attachment limit",
                dropped_file.filename,
                MAX_DROPPED_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        total_bytes += bytes.len() as u64;
        if total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES {
            return Err(format!(
                "Attachments exceed the {} MiB total limit",
                MAX_DROPPED_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
            ));
        }
        let size_bytes = bytes.len() as u64;
        attachments.push(DroppedAttachment {
            mime_type: mime_type_for_filename(&dropped_file.filename).into(),
            filename: dropped_file.filename,
            content_base64: STANDARD.encode(bytes),
            size_bytes,
        });
    }
    Ok(attachments)
}

#[tauri::command]
async fn read_dropped_files(
    window: tauri::WebviewWindow,
    receipts: State<'_, Arc<DroppedFileReceiptStore>>,
    receipt: String,
) -> Result<Vec<DroppedAttachment>, String> {
    let files = receipts.consume(&receipt, window.label())?;
    tokio::task::spawn_blocking(move || materialize_dropped_files(files))
        .await
        .map_err(error)?
}

fn mime_type_for_filename(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => "text/csv",
        "gif" => "image/gif",
        "jpeg" | "jpg" => "image/jpeg",
        "json" => "application/json",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "webp" => "image/webp",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod dropped_file_receipt_tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture_path(directory: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).expect("dropped-file fixture");
        path
    }

    fn issue(
        store: &DroppedFileReceiptStore,
        window_label: &str,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> String {
        store
            .issue_at(window_label, paths, now)
            .expect("native drop receipt")
    }

    #[test]
    fn rejects_forged_receipts_and_raw_paths() {
        let store = DroppedFileReceiptStore::default();
        assert!(store.consume("forged", "compose-1").is_err());
        assert!(store
            .consume("/Users/example/private-file", "compose-1")
            .is_err());
    }

    #[test]
    fn consumes_a_receipt_only_once() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let path = fixture_path(&directory, "drop.txt", b"original");
        let receipt = issue(&store, "compose-1", vec![path.clone()], now);

        let files = store
            .consume_at(&receipt, "compose-1", now)
            .expect("first redemption");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "drop.txt");
        assert!(store.consume_at(&receipt, "compose-1", now).is_err());
    }

    #[test]
    fn rejects_expired_receipts() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let receipt = issue(
            &store,
            "compose-1",
            vec![fixture_path(&directory, "drop.txt", b"original")],
            now,
        );

        assert!(store
            .consume_at(&receipt, "compose-1", now + DROPPED_FILE_RECEIPT_TTL)
            .is_err());
    }

    #[test]
    fn passive_expiry_drops_an_unredeemed_handle() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let receipt = issue(
            &store,
            "compose-1",
            vec![fixture_path(&directory, "drop.txt", b"original")],
            now,
        );

        store.expire_at(&receipt, now + DROPPED_FILE_RECEIPT_TTL);

        assert!(store.consume_at(&receipt, "compose-1", now).is_err());
        assert!(store.entries.lock().expect("receipt store").is_empty());
    }

    #[test]
    fn binds_receipts_to_the_originating_window_session() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let path = fixture_path(&directory, "drop.txt", b"original");
        let receipt = issue(&store, "compose-origin", vec![path.clone()], now);

        assert!(store.consume_at(&receipt, "compose-attacker", now).is_err());
        let files = store
            .consume_at(&receipt, "compose-origin", now)
            .expect("originating window can still redeem");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "drop.txt");
    }

    #[test]
    fn revokes_receipts_when_the_window_session_is_destroyed() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let receipt = store
            .issue(
                "compose-closed",
                vec![fixture_path(&directory, "drop.txt", b"original")],
            )
            .expect("receipt");

        store.revoke_window("compose-closed");

        assert!(store.consume(&receipt, "compose-closed").is_err());
    }

    #[test]
    fn rejects_more_than_the_attachment_count_limit_at_issuance() {
        let store = DroppedFileReceiptStore::default();
        let paths = (0..=MAX_DROPPED_ATTACHMENTS)
            .map(|index| PathBuf::from(format!("/native/{index}")))
            .collect();

        assert!(store.issue("compose-1", paths).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let target = directory.path().join("private.txt");
        let link = directory.path().join("dropped.txt");
        std::fs::write(&target, b"secret").expect("target");
        symlink(&target, &link).expect("symlink");

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", vec![link])
            .expect_err("symlink rejected at native drop");

        assert!(error.contains("regular files"));
    }

    #[test]
    fn rejects_total_size_before_reading_file_contents() {
        let directory = tempdir().expect("tempdir");
        let each_size = 18 * 1024 * 1024;
        let paths = (0..3)
            .map(|index| {
                let path = directory.path().join(format!("{index}.bin"));
                std::fs::File::create(&path)
                    .expect("sparse file")
                    .set_len(each_size)
                    .expect("sparse size");
                path
            })
            .collect();

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", paths)
            .expect_err("aggregate attachment size rejected at native drop");

        assert!(error.contains("total limit"));
    }

    #[test]
    fn rejects_a_file_over_the_per_attachment_limit() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("oversized.bin");
        std::fs::File::create(&path)
            .expect("sparse file")
            .set_len(MAX_DROPPED_ATTACHMENT_BYTES + 1)
            .expect("sparse size");

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", vec![path])
            .expect_err("per-file size limit rejected at native drop");

        assert!(error.contains("attachment limit"));
    }

    #[test]
    fn reads_the_opened_regular_file_handle() {
        let directory = tempdir().expect("tempdir");
        let path = fixture_path(&directory, "notes.txt", b"hello");
        let store = DroppedFileReceiptStore::default();
        let receipt = store
            .issue("compose-1", vec![path])
            .expect("native drop receipt");
        let files = store
            .consume(&receipt, "compose-1")
            .expect("receipt redemption");

        let attachments = materialize_dropped_files(files).expect("attachment");

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "notes.txt");
        assert_eq!(attachments[0].mime_type, "text/plain");
        assert_eq!(attachments[0].size_bytes, 5);
        assert_eq!(
            STANDARD
                .decode(&attachments[0].content_base64)
                .expect("base64"),
            b"hello"
        );
    }

    #[test]
    fn path_replacement_after_issuance_cannot_change_the_opened_file() {
        let directory = tempdir().expect("tempdir");
        let path = fixture_path(&directory, "drop.txt", b"original bytes");
        let moved_original = directory.path().join("moved-original.txt");
        let store = DroppedFileReceiptStore::default();
        let receipt = store
            .issue("compose-1", vec![path.clone()])
            .expect("native drop receipt");

        std::fs::rename(&path, moved_original).expect("move original inode");
        std::fs::write(&path, b"replacement secret").expect("replace dropped path");

        let files = store
            .consume(&receipt, "compose-1")
            .expect("receipt redemption");
        let attachments = materialize_dropped_files(files).expect("attachment");

        assert_eq!(
            STANDARD
                .decode(&attachments[0].content_base64)
                .expect("base64"),
            b"original bytes"
        );
    }
}

fn save_to_downloads(
    app: &tauri::AppHandle,
    attachment: &Attachment,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let downloads = app.path().download_dir()?;
    let filename = dakia_core::mail::safe_attachment_filename(&attachment.filename, 0);
    Ok(save_private_download(&downloads, &filename, bytes)?
        .to_string_lossy()
        .into_owned())
}

fn save_eml_to_downloads(
    app: &tauri::AppHandle,
    subject: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let downloads = app.path().download_dir()?;
    Ok(
        save_private_download(&downloads, &eml_export_filename(subject), bytes)?
            .to_string_lossy()
            .into_owned(),
    )
}

fn eml_export_filename(subject: &str) -> String {
    let sanitized = dakia_core::mail::safe_attachment_filename(subject, 0);
    let fallback = subject
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .all(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '.' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        });
    let stem = if fallback { "message" } else { &sanitized };
    format!("{}.eml", truncate_utf8_filename_stem(stem))
}

fn truncate_utf8_filename_stem(stem: &str) -> &str {
    let maximum_stem_bytes =
        MAX_EXPORT_FILENAME_BYTES - ".eml".len() - MAX_DOWNLOAD_COLLISION_SUFFIX_BYTES;
    if stem.len() <= maximum_stem_bytes {
        return stem;
    }
    let mut end = 0;
    for (index, character) in stem.char_indices() {
        let next = index + character.len_utf8();
        if next > maximum_stem_bytes {
            break;
        }
        end = next;
    }
    &stem[..end]
}

fn save_private_download(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(downloads)?;
    for counter in 0..10_000 {
        let candidate = downloads.join(download_name(filename, counter));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate) {
            Ok(mut file) => {
                if let Err(write_error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(write_error.into());
                }
                #[cfg(unix)]
                if let Err(permission_error) =
                    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))
                {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(permission_error.into());
                }
                return Ok(candidate);
            }
            Err(open_error) if open_error.kind() == ErrorKind::AlreadyExists => continue,
            Err(open_error) => return Err(open_error.into()),
        }
    }
    Err(anyhow::anyhow!(
        "could not choose a safe filename in Downloads"
    ))
}

fn download_name(filename: &str, counter: usize) -> String {
    if counter == 0 {
        return filename.to_owned();
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let extension = path.extension().and_then(|value| value.to_str());
    match extension {
        Some(extension) if !extension.is_empty() => format!("{stem} ({counter}).{extension}"),
        _ => format!("{stem} ({counter})"),
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn eml_export_filename_sanitizes_subjects_and_has_a_safe_fallback() {
        assert_eq!(
            eml_export_filename("Quarterly report: Tallinn"),
            "Quarterly report Tallinn.eml"
        );
        assert_eq!(eml_export_filename("../../\r\n"), "message.eml");
    }

    #[test]
    fn eml_export_filename_stays_within_the_filesystem_byte_limit() {
        let filename = eml_export_filename(&"€".repeat(180));

        assert!(filename.len() <= MAX_EXPORT_FILENAME_BYTES);
        assert!(filename.ends_with(".eml"));
        assert_eq!(filename.trim_end_matches(".eml").len(), 180);
        assert!(
            download_name(&filename, 9999).len() <= MAX_EXPORT_FILENAME_BYTES,
            "the longest collision suffix must still fit"
        );
    }

    #[test]
    fn private_download_keeps_bytes_and_chooses_a_unique_eml_name() {
        let directory = tempdir().expect("temporary Downloads directory");
        let raw = b"From: sender@example.test\r\nSubject: folded\r\n value\r\n\r\nopaque\0\xff";
        let first =
            save_private_download(directory.path(), "status.eml", raw).expect("first export");
        let second = save_private_download(directory.path(), "status.eml", b"second")
            .expect("second export");

        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("status.eml")
        );
        assert_eq!(
            second.file_name().and_then(|name| name.to_str()),
            Some("status (1).eml")
        );
        assert_eq!(std::fs::read(&first).expect("first export bytes"), raw);
        assert_eq!(
            std::fs::read(&second).expect("second export bytes"),
            b"second"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&first)
                .expect("first export metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn export_identity_rejects_uid_reuse_after_a_mailbox_epoch_change() {
        let before = MailSummary {
            id: "message".into(),
            account_id: Uuid::nil().to_string(),
            mailbox: "INBOX".into(),
            uid: 42,
            message_id: Some("<old@example.test>".into()),
            in_reply_to: None,
            reference_ids: None,
            thread_id: "thread".into(),
            subject: "Old".into(),
            from_name: None,
            from_address: "old@example.test".into(),
            to_addresses: "me@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: String::new(),
            body_text: String::new(),
            body_html: None,
            content_state: "headers_only".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: false,
            is_flagged: false,
            is_answered: false,
            is_draft: false,
            has_attachments: false,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: vec![],
        };
        let mut reused_uid = before.clone();
        reused_uid.message_id = Some("<new@example.test>".into());
        reused_uid.received_at += chrono::Duration::seconds(1);

        assert!(same_export_identity(&before, &before));
        assert!(!same_export_identity(&before, &reused_uid));
    }
}

fn install_app_menu(app: &tauri::App) -> tauri::Result<()> {
    let settings = MenuItemBuilder::with_id("settings", "Settings…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let check_for_updates =
        MenuItemBuilder::with_id("check-for-updates", "Check for Updates…").build(app)?;
    let new_message = MenuItemBuilder::with_id("new-message", "New Message")
        .accelerator("CmdOrCtrl+N")
        .build(app)?;
    let add_account = MenuItemBuilder::with_id("add-account", "Add Account…").build(app)?;
    let search = MenuItemBuilder::with_id("search", "Find in Mailbox")
        .accelerator("CmdOrCtrl+F")
        .build(app)?;
    let sync = MenuItemBuilder::with_id("sync", "Get New Mail")
        .accelerator("CmdOrCtrl+Shift+N")
        .build(app)?;
    let reply = MenuItemBuilder::with_id("reply", "Reply")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let forward = MenuItemBuilder::with_id("forward", "Forward")
        .accelerator("CmdOrCtrl+Shift+F")
        .build(app)?;
    let archive = MenuItemBuilder::with_id("archive", "Archive")
        .accelerator("CmdOrCtrl+Shift+A")
        .build(app)?;
    let spam = MenuItemBuilder::with_id("spam", "Mark as Junk")
        .accelerator("CmdOrCtrl+Shift+J")
        .build(app)?;
    let keyboard_shortcuts =
        MenuItemBuilder::with_id("keyboard-shortcuts", "Keyboard Shortcuts").build(app)?;
    #[cfg(target_os = "macos")]
    let terminal_command = {
        let label = app
            .path()
            .resource_dir()
            .ok()
            .and_then(|_| std::env::current_exe().ok())
            .and_then(|path| path.parent().map(|parent| parent.join("dakia")))
            .filter(|path| {
                matches!(
                    terminal_command_status_for(path),
                    TerminalCommandStatus::Available
                )
            })
            .map(|_| "Remove Dakia Terminal Command…")
            .unwrap_or("Use Dakia from Terminal…");
        MenuItemBuilder::with_id("terminal-command", label).build(app)?
    };

    let app_menu_builder = SubmenuBuilder::new(app, "Dakia")
        .about(None)
        .separator()
        .item(&settings)
        .item(&check_for_updates);
    #[cfg(target_os = "macos")]
    let app_menu_builder = app_menu_builder.separator().item(&terminal_command);
    let app_menu = app_menu_builder
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;
    let file_menu = SubmenuBuilder::new(app, "File")
        .item(&new_message)
        .item(&add_account)
        .separator()
        .close_window()
        .build()?;
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let mailbox_menu = SubmenuBuilder::new(app, "Mailbox")
        .item(&sync)
        .separator()
        .item(&search)
        .build()?;
    let message_menu = SubmenuBuilder::new(app, "Message")
        .item(&reply)
        .item(&forward)
        .separator()
        .item(&archive)
        .item(&spam)
        .build()?;
    let window_menu = SubmenuBuilder::with_id(app, "window", "Window")
        .minimize()
        .maximize()
        .fullscreen()
        .separator()
        .bring_all_to_front()
        .build()?;
    let help_menu = SubmenuBuilder::new(app, "Help")
        .item(&keyboard_shortcuts)
        .build()?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &app_menu,
            &file_menu,
            &edit_menu,
            &mailbox_menu,
            &message_menu,
            &window_menu,
            &help_menu,
        ])
        .build()?;
    app.set_menu(menu)?;
    Ok(())
}

fn install_tray(app: &tauri::AppHandle, open_label: &str, quit_label: &str) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("tray-open", open_label).build(app)?;
    let quit = MenuItemBuilder::with_id("tray-quit", quit_label).build(app)?;
    let menu = MenuBuilder::new(app).items(&[&open, &quit]).build()?;
    if let Some(tray) = app.tray_by_id("dakia-tray") {
        tray.set_menu(Some(menu))?;
        return Ok(());
    }
    let tray_icon = Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
    let builder = TrayIconBuilder::with_id("dakia-tray")
        .icon(tray_icon)
        .icon_as_template(true)
        .tooltip("Dakia")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-open" => show_main_window(app),
            "tray-quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    app.state::<Arc<AppState>>().realtime.stop_all().await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    builder.build(app)?;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn menu_action_targets_focused_window(action: &str) -> bool {
    matches!(action, "reply" | "forward" | "archive" | "spam")
        || action.starts_with("copy-email-address:")
        || action.starts_with("compose-email-address:")
}

#[tauri::command]
async fn provider_presets() -> Vec<ProviderPreset> {
    provider::all().to_vec()
}

#[tauri::command]
fn configure_tray(
    app: tauri::AppHandle,
    open_label: String,
    quit_label: String,
) -> Result<(), String> {
    if open_label.trim().is_empty() || quit_label.trim().is_empty() {
        return Err("Tray labels are required".into());
    }
    install_tray(&app, open_label.trim(), quit_label.trim()).map_err(error)
}

#[tauri::command]
async fn accounts(state: State<'_, Arc<AppState>>) -> Result<Vec<Account>, String> {
    state.store.accounts().await.map_err(error)
}

/// Search folder scopes are account-owned provider identities, not inferred
/// labels. Return every enabled account's durable discovery rows so callers
/// can distinguish non-selectable hierarchy nodes from searchable mailboxes.
#[tauri::command]
async fn list_search_mailboxes(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<SelectableMailbox>, String> {
    let accounts = state.store.accounts().await.map_err(error)?;
    let mut mailboxes = Vec::new();
    for account in accounts.into_iter().filter(|account| account.enabled) {
        mailboxes.extend(
            state
                .store
                .list_selectable_mailboxes(account.id)
                .await
                .map_err(error)?,
        );
    }
    Ok(sort_and_deduplicate_search_mailboxes(mailboxes))
}

fn sort_and_deduplicate_search_mailboxes(
    mut mailboxes: Vec<SelectableMailbox>,
) -> Vec<SelectableMailbox> {
    mailboxes.sort_by(|left, right| {
        left.account_id
            .cmp(&right.account_id)
            .then_with(|| left.local_path.cmp(&right.local_path))
            .then_with(|| left.remote_path.cmp(&right.remote_path))
            .then_with(|| left.id.cmp(&right.id))
    });
    mailboxes.dedup_by(|left, right| {
        left.account_id == right.account_id && left.remote_path == right.remote_path
    });
    mailboxes
}

#[tauri::command]
async fn update_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    input: UpdateAccountInput,
) -> Result<Account, String> {
    if input.account_name.trim().is_empty() {
        return Err("Account name is required".into());
    }
    if input.display_name.trim().is_empty() {
        return Err("Your name is required".into());
    }
    if input.imap_host.trim().is_empty() || input.smtp_host.trim().is_empty() {
        return Err("IMAP and SMTP hosts are required".into());
    }
    // Ask an existing rebuild to stop before waiting for its account lock. A
    // normal settings or credential update retains the durable job so it can
    // resume with the new connection details.
    request_mail_rebuild_cancel(
        state.inner(),
        input.id,
        MailRebuildCancellationDisposition::Retain,
    );
    let _operation = state.account_operations.acquire(input.id).await;
    invalidate_searches_for_account(state.inner(), input.id).await?;
    let mut account = state
        .store
        .account(input.id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    let previous_account = account.clone();
    account.account_name = input.account_name.trim().to_owned();
    account.display_name = input.display_name.trim().to_owned();
    account.imap_host = input.imap_host.trim().to_owned();
    account.imap_port = input.imap_port;
    account.imap_security = input.imap_security;
    account.smtp_host = input.smtp_host.trim().to_owned();
    account.smtp_port = input.smtp_port;
    account.smtp_security = input.smtp_security;
    account.archive_mailbox = input.archive_mailbox.trim().to_owned();
    account.spam_mailbox = input.spam_mailbox.trim().to_owned();
    let password = input.password.filter(|value| !value.trim().is_empty());
    let password_was_supplied = password.is_some();
    let converts_legacy_oauth =
        password_was_supplied && matches!(&previous_account.auth, AccountAuth::OAuth2 { .. });
    if converts_legacy_oauth {
        if let Err(validation_error) =
            validate_legacy_oauth_conversion(&previous_account, &state.realtime.statuses().await)
        {
            resume_scheduled_mail_rebuild(
                app.clone(),
                state.inner().clone(),
                previous_account.clone(),
            )
            .await;
            return Err(validation_error);
        }
    } else if password_was_supplied && !matches!(account.auth, AccountAuth::Password { .. }) {
        resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account.clone())
            .await;
        return Err("OAuth accounts can only be converted after authentication fails".into());
    }
    let namespace_changed = !same_mail_namespace(&previous_account, &account);
    if converts_legacy_oauth && namespace_changed {
        resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account.clone())
            .await;
        return Err(
            "Save the Google app password before changing the IMAP or folder settings".into(),
        );
    }
    if namespace_changed || converts_legacy_oauth {
        // Stop the old namespace watcher before committing a replacement.
        state.realtime.stop_account(account.id).await;
    }
    let password_secret_name = credential_secret_name(&account);

    if converts_legacy_oauth {
        convert_legacy_oauth_to_password(&mut account);
        if let Err(save_error) = state
            .store
            .save_account_with_secret(
                &account,
                &password_secret_name,
                password.as_deref().expect("conversion password"),
            )
            .await
        {
            if previous_account.enabled {
                state
                    .realtime
                    .start_account(app.clone(), previous_account.clone())
                    .await;
            }
            resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account)
                .await;
            return Err(error(save_error));
        }
        if account.enabled {
            state
                .realtime
                .start_account(app.clone(), account.clone())
                .await;
        }
        kick_contacted_people_migrations(state.inner().clone());
        resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
        return Ok(account);
    }

    let previous_password_credential = if password_was_supplied {
        match state.store.secret(&password_secret_name).await {
            Ok(credential) => credential,
            Err(secret_error) => {
                if namespace_changed {
                    let _ = state.realtime.reconcile(app.clone()).await;
                }
                return Err(error(secret_error));
            }
        }
    } else {
        None
    };
    if let Some(password) = password.as_deref() {
        if let Err(set_error) = MailService::new(state.store.clone())
            .credentials()
            .set_password(&account, password)
            .await
        {
            if namespace_changed {
                let _ = state.realtime.reconcile(app.clone()).await;
            }
            resume_scheduled_mail_rebuild(
                app.clone(),
                state.inner().clone(),
                previous_account.clone(),
            )
            .await;
            return Err(error(set_error));
        }
    }
    if let Err(save_error) = save_account_with_rebuild_intent(
        state.inner(),
        &account,
        namespace_changed,
        None,
        &password_secret_name,
    )
    .await
    {
        if password_was_supplied {
            let rollback_mail = MailService::new(state.store.clone());
            let credentials = rollback_mail.credentials();
            let rollback = match previous_password_credential {
                Some(previous) => {
                    state
                        .store
                        .set_secret(&password_secret_name, &previous)
                        .await
                }
                None => credentials.delete(&account).await,
            };
            if let Err(rollback_error) = rollback {
                tracing::error!(account_id = %account.id, error = %rollback_error, "could not roll back password credentials after saving the account failed");
            }
        }
        if namespace_changed {
            let _ = state.realtime.reconcile(app.clone()).await;
        }
        resume_scheduled_mail_rebuild(app, state.inner().clone(), previous_account).await;
        return Err(error(save_error));
    }
    if !namespace_changed {
        state.realtime.reconcile(app.clone()).await.map_err(error)?;
    }
    kick_contacted_people_migrations(state.inner().clone());
    resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
    Ok(account)
}

#[tauri::command]
async fn show_account_context_menu(
    window: tauri::Window,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    rename_label: String,
) -> Result<(), String> {
    if state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .is_none()
    {
        return Err("Account not found".into());
    }
    let rename = MenuItemBuilder::with_id(format!("rename-account:{account_id}"), rename_label)
        .build(&window)
        .map_err(error)?;
    let menu = MenuBuilder::new(&window)
        .item(&rename)
        .build()
        .map_err(error)?;
    window.popup_menu(&menu).map_err(error)
}

fn validated_context_menu_email_address(value: &str) -> Result<&str, String> {
    let address = value.trim();
    if address.is_empty()
        || address.len() > 320
        || address.matches('@').count() != 1
        || address
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("Invalid email address".into());
    }
    Ok(address)
}

fn decode_context_menu_email_address(value: &str) -> Result<String, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "Invalid encoded email address".to_owned())?;
    let address =
        String::from_utf8(bytes).map_err(|_| "Invalid encoded email address".to_owned())?;
    validated_context_menu_email_address(&address)?;
    Ok(address)
}

#[tauri::command]
fn show_email_address_context_menu(
    window: tauri::Window,
    account_id: Uuid,
    address: String,
    copy_label: String,
    new_message_label: String,
) -> Result<(), String> {
    let address = validated_context_menu_email_address(&address)?;
    let encoded_address = URL_SAFE_NO_PAD.encode(address.as_bytes());
    let copy =
        MenuItemBuilder::with_id(format!("copy-email-address:{encoded_address}"), copy_label)
            .build(&window)
            .map_err(error)?;
    let new_message = MenuItemBuilder::with_id(
        format!("compose-email-address:{account_id}:{encoded_address}"),
        new_message_label,
    )
    .build(&window)
    .map_err(error)?;
    let menu = MenuBuilder::new(&window)
        .items(&[&copy, &new_message])
        .build()
        .map_err(error)?;
    window.popup_menu(&menu).map_err(error)
}

#[cfg(test)]
mod email_address_context_menu_tests {
    use super::*;

    #[test]
    fn validates_email_addresses_before_building_menu_ids() {
        assert_eq!(
            validated_context_menu_email_address("  person+tag@example.com  "),
            Ok("person+tag@example.com")
        );
        assert_eq!(
            validated_context_menu_email_address("müller@example.com"),
            Ok("müller@example.com")
        );
        for invalid in [
            "",
            "missing-at.example.com",
            "two@@example.com",
            "person @example.com",
            "person@example.com\tmenu-action",
            "person@example.com\nmenu-action",
            "person@example.com\u{0000}",
        ] {
            assert!(
                validated_context_menu_email_address(invalid).is_err(),
                "accepted invalid address {invalid:?}"
            );
        }

        let maximum = format!("{}@b", "a".repeat(318));
        let too_long = format!("{}@b", "a".repeat(319));
        assert_eq!(maximum.len(), 320);
        assert!(validated_context_menu_email_address(&maximum).is_ok());
        assert!(validated_context_menu_email_address(&too_long).is_err());
    }

    #[test]
    fn menu_address_encoding_is_url_safe_and_round_trips_utf8() {
        let address = "müller+news@example.com";
        let encoded = URL_SAFE_NO_PAD.encode(address.as_bytes());
        assert!(encoded
            .chars()
            .all(|character| !matches!(character, '+' | '/' | '=')));
        assert_eq!(URL_SAFE_NO_PAD.decode(encoded).unwrap(), address.as_bytes());
        assert_eq!(
            decode_context_menu_email_address(&URL_SAFE_NO_PAD.encode(address)),
            Ok(address.to_owned())
        );
        assert!(decode_context_menu_email_address("_w").is_err());
        assert!(
            decode_context_menu_email_address(&URL_SAFE_NO_PAD.encode("not@an@address")).is_err()
        );
    }

    #[test]
    fn address_actions_return_to_the_window_that_opened_the_menu() {
        assert!(menu_action_targets_focused_window(
            "copy-email-address:cGVyc29uQGV4YW1wbGUuY29t"
        ));
        assert!(menu_action_targets_focused_window(
            "compose-email-address:account:cGVyc29uQGV4YW1wbGUuY29t"
        ));
        assert!(menu_action_targets_focused_window("reply"));
        assert!(!menu_action_targets_focused_window("new-message"));
        assert!(!menu_action_targets_focused_window(
            "rename-account:account"
        ));
    }
}

#[tauri::command]
async fn remove_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
) -> Result<(), String> {
    request_mail_rebuild_cancel(
        state.inner(),
        account_id,
        MailRebuildCancellationDisposition::Remove,
    );
    let _operation = state.account_operations.acquire(account_id).await;
    invalidate_searches_for_account(state.inner(), account_id).await?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    // `stop_account` waits for the watcher task to leave IMAP and complete
    // its current storage call before destructive storage work begins.
    state.realtime.stop_account(account_id).await;
    MailService::new(state.store.clone())
        .credentials()
        .delete(&account)
        .await
        .map_err(error)?;
    state
        .store
        .delete_account(account_id)
        .await
        .map_err(error)?;
    kick_contacted_people_migrations(state.inner().clone());
    if let Err(error) = app.emit(
        "account-removed",
        serde_json::json!({ "accountId": account_id }),
    ) {
        tracing::error!(error = %error, "could not notify windows about account removal");
    }
    if let Err(error) = state.realtime.reconcile(app).await {
        tracing::error!(error = %error, "could not reconcile real-time mail after account removal");
    }
    Ok(())
}

#[tauri::command]
fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let parsed = Url::parse(&url).map_err(error)?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return Err("Only web and email links can be opened".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(error)
}

#[tauri::command]
async fn add_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    input: AddAccountInput,
) -> Result<AccountConnection, String> {
    if input.password.trim().is_empty() {
        return Err("A password or app password is required".into());
    }
    let preset = input
        .draft
        .provider_id
        .as_deref()
        .and_then(provider::by_id)
        .unwrap_or_else(|| provider::detect(&input.draft.email));
    if preset.id == "custom"
        && (input
            .draft
            .imap_host
            .as_deref()
            .unwrap_or_default()
            .is_empty()
            || input
                .draft
                .smtp_host
                .as_deref()
                .unwrap_or_default()
                .is_empty())
    {
        return Err("Custom accounts require IMAP and SMTP hosts".into());
    }
    let account = input.draft.into_account(preset);
    let stored_accounts = state.store.accounts().await.map_err(error)?;
    ensure_account_is_not_connected(&stored_accounts, &account.email)?;
    let _operation = state.account_operations.acquire(account.id).await;
    state.realtime.stop_account(account.id).await;
    let mail = MailService::new(state.store.clone());
    let password_secret_name = credential_secret_name(&account);
    if let Err(set_error) = mail
        .credentials()
        .set_password(&account, &input.password)
        .await
    {
        return Err(error(set_error));
    }
    if let Err(save_error) =
        save_account_with_rebuild_intent(state.inner(), &account, true, None, &password_secret_name)
            .await
    {
        let rollback = mail.credentials().delete(&account).await;
        if let Err(rollback_error) = rollback {
            tracing::error!(
                account_id = %account.id,
                error = %rollback_error,
                "could not roll back password credentials after saving the account failed"
            );
        }
        return Err(error(save_error));
    }
    kick_contacted_people_migrations(state.inner().clone());
    resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
    Ok(AccountConnection {
        account,
        reused_existing_account: false,
    })
}

#[tauri::command]
async fn search(
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<MailConversationPage, String> {
    let accounts = state.store.accounts().await.map_err(error)?;
    let Some(query) = legacy_search_query_for_enabled_accounts(query, &accounts) else {
        return Ok(MailConversationPage {
            conversations: Vec::new(),
            match_evidence: Default::default(),
            next_cursor: None,
            candidate_cursor: None,
            candidate_exhausted: true,
        });
    };
    state
        .store
        .search_conversation_page(&query)
        .await
        .map_err(error)
}

#[tauri::command]
async fn search_smart_inbox(
    state: State<'_, Arc<AppState>>,
    query: SmartInboxQuery,
) -> Result<SmartInboxPage, String> {
    state.store.search_smart_inbox(&query).await.map_err(error)
}

#[tauri::command]
async fn suggest_contacted_people(
    state: State<'_, Arc<AppState>>,
    prefix: String,
    account_id: Option<Uuid>,
    limit: Option<u8>,
) -> Result<Vec<dakia_core::storage::ContactedPersonSuggestion>, String> {
    let limit = contacted_people_suggestion_limit(limit)?;
    if let Some(account_id) = account_id {
        // The preferred account affects ranking. Do not leave a deleted or
        // disabled account ID in that ranking request.
        enabled_account_for_operation(state.inner(), account_id).await?;
    }
    locally_suggest_contacted_people(&state.store, &prefix, account_id, limit).await
}

fn contacted_people_suggestion_limit(limit: Option<u8>) -> Result<usize, String> {
    let limit = limit.unwrap_or(8);
    if limit > 8 {
        return Err("Contacted-people suggestion limit cannot exceed 8".to_owned());
    }
    Ok(usize::from(limit))
}

async fn locally_suggest_contacted_people(
    store: &Store,
    prefix: &str,
    account_id: Option<Uuid>,
    limit: usize,
) -> Result<Vec<dakia_core::storage::ContactedPersonSuggestion>, String> {
    if !store
        .autocomplete_suggestions_enabled()
        .await
        .map_err(error)?
    {
        return Ok(Vec::new());
    }
    let mut people = store
        .suggest_contacted_people(prefix, account_id)
        .await
        .map_err(error)?;
    people.truncate(limit);
    Ok(people)
}

#[tauri::command]
async fn hide_contacted_person(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    address: String,
) -> Result<(), String> {
    let enabled = state
        .store
        .autocomplete_suggestions_enabled()
        .await
        .map_err(error)?;
    state
        .store
        .hide_contacted_person(&address)
        .await
        .map_err(error)?;
    emit_contacted_people_changed(&app, enabled, false);
    Ok(())
}

#[tauri::command]
async fn clear_contacted_people(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let enabled = state
        .store
        .autocomplete_suggestions_enabled()
        .await
        .map_err(error)?;
    state.store.clear_contacted_people().await.map_err(error)?;
    emit_contacted_people_changed(&app, enabled, true);
    Ok(())
}

#[tauri::command]
async fn get_autocomplete_settings(
    state: State<'_, Arc<AppState>>,
) -> Result<ContactedPeopleSettings, String> {
    Ok(ContactedPeopleSettings {
        enabled: state
            .store
            .autocomplete_suggestions_enabled()
            .await
            .map_err(error)?,
    })
}

#[tauri::command]
async fn set_autocomplete_settings(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    enabled: bool,
) -> Result<ContactedPeopleSettings, String> {
    state
        .store
        .set_autocomplete_suggestions_enabled(enabled)
        .await
        .map_err(error)?;
    emit_contacted_people_changed(&app, enabled, false);
    Ok(ContactedPeopleSettings { enabled })
}

fn emit_contacted_people_changed(app: &tauri::AppHandle, enabled: bool, cleared: bool) {
    if let Err(error) = app.emit(
        "contacted-people-changed",
        ContactedPeopleChanged { enabled, cleared },
    ) {
        tracing::warn!(error = %error, "could not notify windows about contacted-people changes");
    }
}

/// Validates recipient fields with the same Rust parser used to build the
/// SMTP envelope. This boundary is intentionally synchronous and local: it
/// cannot send mail or collect contacted-person history.
#[tauri::command]
fn validate_compose_recipients(
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
) -> dakia_core::ComposeRecipientValidation {
    dakia_core::validate_compose_recipients(&to, &cc, &bcc)
}

#[cfg(test)]
mod contacted_people_command_tests {
    use super::*;
    use dakia_core::storage::ContactedPersonRecipient;
    use std::sync::atomic::AtomicUsize;

    struct NoopClassifier;

    impl EmailClassifier for NoopClassifier {
        fn classify(
            &mut self,
            _emails: &[EmailClassificationInput],
        ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
            Ok(Vec::new())
        }
    }

    fn contacted_people_test_state(store: Store) -> Arc<AppState> {
        Arc::new(AppState {
            realtime: RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: PathBuf::new(),
            classifier: Mutex::new(Box::new(NoopClassifier)),
            classification_owner: "contacted-people-test".into(),
            classification: Arc::new(ClassificationScheduler::default()),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: MailRebuildCancellations::default(),
            account_operations: AccountOperationLocks::default(),
            search_sessions: SearchSessionRegistry::default(),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            remote_search_slots: Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY)),
            translation_downloads: Mutex::new(HashMap::new()),
            contacted_people_migration_drain: Arc::new(AsyncMutex::new(())),
        })
    }

    async fn account_with_contacted_person() -> (Store, Account) {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "sender@example.test".into(),
            display_name: "Sender".into(),
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
        store.save_account(&account).await.unwrap();
        store
            .record_successful_outgoing_recipients(
                account.id,
                &[ContactedPersonRecipient {
                    address: "recipient@example.test".into(),
                    display_name: Some("Recipient".into()),
                    formatted_address: Some("Recipient <recipient@example.test>".into()),
                }],
                std::slice::from_ref(&account.email),
            )
            .await
            .unwrap();
        (store, account)
    }

    #[tokio::test]
    async fn contacted_people_suggestions_honor_setting_and_limit() {
        let (store, account) = account_with_contacted_person().await;
        assert_eq!(contacted_people_suggestion_limit(None).unwrap(), 8);
        assert_eq!(contacted_people_suggestion_limit(Some(1)).unwrap(), 1);
        assert!(contacted_people_suggestion_limit(Some(9)).is_err());

        let suggestions = locally_suggest_contacted_people(
            &store,
            "recipient",
            Some(account.id),
            contacted_people_suggestion_limit(Some(1)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].address, "recipient@example.test");

        store
            .set_autocomplete_suggestions_enabled(false)
            .await
            .unwrap();
        assert!(
            locally_suggest_contacted_people(&store, "", Some(account.id), 8)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn autocomplete_settings_use_the_frontend_payload_shape() {
        assert_eq!(
            serde_json::to_value(ContactedPeopleSettings { enabled: true }).unwrap(),
            serde_json::json!({ "enabled": true })
        );
    }

    #[tokio::test]
    async fn contacted_people_background_worker_drains_more_than_one_batch_in_one_lifecycle() {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "sender@example.test".into(),
            display_name: "Sender".into(),
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
        store.save_account(&account).await.unwrap();
        let sent = (1..=101)
            .map(|uid| MailSummary {
                id: format!("sent-backfill-{uid}"),
                account_id: account.id.to_string(),
                mailbox: "Sent".into(),
                uid,
                message_id: Some(format!("<sent-backfill-{uid}@example.test>")),
                in_reply_to: None,
                reference_ids: None,
                thread_id: format!("sent-backfill-thread-{uid}"),
                subject: "Sent message".into(),
                from_name: Some("Sender".into()),
                from_address: account.email.clone(),
                to_addresses: format!("Person {uid} <person{uid}@example.test>"),
                cc_addresses: String::new(),
                bcc_addresses: String::new(),
                reply_to_addresses: String::new(),
                received_at: chrono::Utc::now(),
                snippet: String::new(),
                body_text: String::new(),
                body_html: None,
                content_state: "headers_only".into(),
                unsubscribe_kind: None,
                unsubscribe_url: None,
                is_read: true,
                is_flagged: false,
                is_answered: false,
                is_draft: false,
                has_attachments: false,
                category: None,
                classification_confidence: None,
                classification_source: None,
                classification_signals: String::new(),
                attachments: Vec::new(),
            })
            .collect::<Vec<_>>();
        store.upsert_catalog_messages(&sent).await.unwrap();
        let state = contacted_people_test_state(store.clone());
        let notifications = Arc::new(AtomicUsize::new(0));

        drain_contacted_people_backfill(&state, {
            let notifications = notifications.clone();
            move || {
                notifications.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;

        let complete = store
            .backfill_contacted_people_from_sent(
                account.id,
                std::slice::from_ref(&account.email),
                100,
            )
            .await
            .unwrap();
        assert!(complete.complete);
        assert_eq!(complete.processed_messages, 0);
        assert!(notifications.load(Ordering::SeqCst) >= 2);
        assert_eq!(
            store
                .suggest_contacted_people("person101", Some(account.id))
                .await
                .unwrap()
                .first()
                .map(|person| person.address.as_str()),
            Some("person101@example.test")
        );
    }

    #[tokio::test]
    async fn runtime_account_disable_restarts_contacted_people_migration_drain_after_startup() {
        let store = Store::in_memory().await.unwrap();
        let active = AccountDraft {
            email: "runtime-active@example.test".into(),
            display_name: "Runtime active".into(),
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
        let mut disabled = AccountDraft {
            email: "runtime-disabled@example.test".into(),
            display_name: "Runtime disabled".into(),
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
        store.save_account(&active).await.unwrap();
        store.save_account(&disabled).await.unwrap();

        // Use the public accepted-send path. The final recipient is row 501,
        // so save_account's first bounded storage batch cannot refresh it.
        for index in 0..=500 {
            let address = format!("runtime{index:03}@example.test");
            let display_name = if index == 500 {
                "Bob Final".to_string()
            } else {
                format!("Bob {index:03}")
            };
            store
                .record_successful_outgoing_recipients(
                    active.id,
                    &[ContactedPersonRecipient {
                        address,
                        display_name: Some(display_name),
                        formatted_address: None,
                    }],
                    std::slice::from_ref(&active.email),
                )
                .await
                .unwrap();
        }

        store
            .record_successful_outgoing_recipients(
                disabled.id,
                &[ContactedPersonRecipient {
                    address: "runtime500@example.test".into(),
                    display_name: Some("Alice Disabled".into()),
                    formatted_address: None,
                }],
                &[disabled.email.clone()],
            )
            .await
            .unwrap();
        let state = contacted_people_test_state(store.clone());
        disabled.enabled = false;
        store.save_account(&disabled).await.unwrap();
        // Startup's original worker is already absent. A runtime mutation
        // must schedule a fresh serialized drain for the remaining batch.
        // Two callers may race to request a continuation. The shared mutex
        // makes the second one wait, then observe the durable complete marker.
        kick_contacted_people_migrations(state.clone());
        kick_contacted_people_migrations(state.clone());
        for _ in 0..100 {
            let final_person = store
                .suggest_contacted_people("runtime500", Some(active.id))
                .await
                .unwrap();
            if final_person
                .first()
                .and_then(|person| person.display_name.as_deref())
                == Some("Bob Final")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let final_person = store
            .suggest_contacted_people("runtime500", Some(active.id))
            .await
            .unwrap();
        assert_eq!(
            final_person
                .first()
                .and_then(|person| person.display_name.as_deref()),
            Some("Bob Final"),
            "runtime drain converges without reopening the store"
        );

        // Exercise the same runtime path for deletion. Re-enabling makes the
        // latest disabled-account form authoritative again; deletion must
        // drain all 501 rows back to the remaining account's form.
        disabled.enabled = true;
        store.save_account(&disabled).await.unwrap();
        kick_contacted_people_migrations(state.clone());
        for _ in 0..100 {
            let final_person = store
                .suggest_contacted_people("runtime500", Some(active.id))
                .await
                .unwrap();
            if final_person
                .first()
                .and_then(|person| person.display_name.as_deref())
                == Some("Alice Disabled")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            store
                .suggest_contacted_people("runtime500", Some(active.id))
                .await
                .unwrap()
                .first()
                .and_then(|person| person.display_name.as_deref()),
            Some("Alice Disabled")
        );

        store.delete_account(disabled.id).await.unwrap();
        kick_contacted_people_migrations(state.clone());
        for _ in 0..100 {
            let final_person = store
                .suggest_contacted_people("runtime500", Some(active.id))
                .await
                .unwrap();
            if final_person
                .first()
                .and_then(|person| person.display_name.as_deref())
                == Some("Bob Final")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            store
                .suggest_contacted_people("runtime500", Some(active.id))
                .await
                .unwrap()
                .first()
                .and_then(|person| person.display_name.as_deref()),
            Some("Bob Final"),
            "deleting an account also converges without reopening the store"
        );
    }
}

#[tauri::command]
async fn conversation_for_target(
    state: State<'_, Arc<AppState>>,
    target: ConversationTarget,
) -> Result<Option<MailConversation>, String> {
    state
        .store
        .conversation_for_target(&target)
        .await
        .map_err(error)
}

fn search_v2_error(error: impl std::fmt::Display) -> SearchErrorV2 {
    // Provider and SQLite errors can include hosts, mailbox names, SQL, or
    // server response fragments. Keep those in the native log but never copy
    // them into an IPC error rendered by the UI.
    tracing::warn!(error = %error, "search request failed");
    SearchErrorV2 {
        position: None,
        category: SearchErrorCategory::Transient,
        unsupported_operator: None,
        message: "Search could not be completed. Please try again.".into(),
    }
}

fn emit_search_progress(
    app: &tauri::AppHandle,
    sessions: &SearchSessionRegistry,
    session: &SearchSession,
    coverage: Vec<SearchCoverage>,
) {
    if coverage.is_empty() || !sessions.is_current(session) {
        return;
    }
    let _ = app.emit(
        "search-progress",
        SearchProgressUpdate {
            session_id: session.session_id,
            revision: session.revision,
            coverage,
        },
    );
}

fn expression_has_folder_predicate(expression: &dakia_core::SearchExpression) -> bool {
    fn visit(node: &dakia_core::SearchNode) -> bool {
        match node {
            dakia_core::SearchNode::MatchAll => false,
            dakia_core::SearchNode::Term(dakia_core::SearchTerm::Folder(_)) => true,
            dakia_core::SearchNode::Term(_) => false,
            dakia_core::SearchNode::And(nodes) | dakia_core::SearchNode::Or(nodes) => {
                nodes.iter().any(visit)
            }
            dakia_core::SearchNode::Not(node) => visit(node),
        }
    }

    visit(&expression.root)
}

/// `scope.mailbox` is ambient UI state, not a second folder predicate. A
/// submitted `in:` expression is explicit user intent and must replace that
/// ambient scope for local SQL and provider mailbox planning alike.
fn effective_search_scope_mailbox(
    request: &SearchRequestV2,
    expression: &dakia_core::SearchExpression,
) -> Option<String> {
    (!expression_has_folder_predicate(expression)).then(|| request.scope.mailbox.clone())?
}

fn search_v2_query(
    request: &SearchRequestV2,
    account_ids: Vec<Uuid>,
    expression: &dakia_core::SearchExpression,
) -> SearchQuery {
    // `in:*` is the canonical explicit opt-in to the normally hidden Spam and
    // Trash families. Keep the caller's raw query opaque to TypeScript while
    // preserving the parser as the only syntax authority.
    let has_explicit_folder = expression_has_folder_predicate(expression);
    let text = if request.scope.include_spam_trash && !has_explicit_folder {
        if request.raw_query.trim().is_empty() {
            "in:*".to_owned()
        } else {
            format!("({}) in:*", request.raw_query)
        }
    } else {
        request.raw_query.clone()
    };
    SearchQuery {
        text,
        account_ids,
        mailbox: effective_search_scope_mailbox(request, expression),
        limit: Some(request.effective_page_size()),
        ..SearchQuery::default()
    }
}

fn provider_coverage_for_error(account_id: Uuid, error: &str) -> SearchCoverage {
    let lower = error.to_ascii_lowercase();
    let state = if lower.contains("identity changed") || lower.contains("uidvalidity") {
        SearchCoverageState::MailboxChanged
    } else if lower.contains("authentication") || lower.contains("credential") {
        SearchCoverageState::AuthenticationFailed
    } else {
        SearchCoverageState::Offline
    };
    SearchCoverage {
        account_id,
        mailbox: None,
        state,
        detail: Some("Provider search was unavailable for this account.".into()),
    }
}

fn provider_mailbox_coverage(
    account_id: Uuid,
    mailbox: String,
    provider_state: ProviderMailboxSearchState,
) -> SearchCoverage {
    let (state, detail) = match provider_state {
        ProviderMailboxSearchState::Searched => (SearchCoverageState::ProviderSearched, None),
        ProviderMailboxSearchState::Partial => (
            SearchCoverageState::ProviderPartial,
            Some("Some provider candidates could not be fully verified for this mailbox.".into()),
        ),
        ProviderMailboxSearchState::SearchBodyCacheIncomplete => (
            SearchCoverageState::LocalBodyIndex,
            Some("Provider results were found, but some text is not available for later local pages.".into()),
        ),
        ProviderMailboxSearchState::MailboxChanged => (
            SearchCoverageState::MailboxChanged,
            Some("This mailbox changed while it was being searched.".into()),
        ),
        ProviderMailboxSearchState::Offline => (
            SearchCoverageState::Offline,
            Some("Provider search was unavailable for this mailbox.".into()),
        ),
    };
    SearchCoverage {
        account_id,
        mailbox: Some(mailbox),
        state,
        detail,
    }
}

async fn local_search_coverage(
    store: &Store,
    account_ids: &[Uuid],
    mailbox: Option<String>,
    progress: &dakia_core::storage::SearchCatalogueV2BackfillProgress,
) -> anyhow::Result<Vec<SearchCoverage>> {
    let mut coverage = Vec::with_capacity(account_ids.len());
    for account_id in account_ids.iter().copied() {
        let mailboxes = store.list_selectable_mailboxes(account_id).await?;
        let scoped_mailboxes = mailboxes
            .iter()
            .filter(|candidate| {
                candidate.selectable
                    && mailbox.as_ref().is_none_or(|scope| {
                        candidate.local_path == *scope || candidate.remote_path == *scope
                    })
            })
            .collect::<Vec<_>>();
        let incomplete_mailboxes = scoped_mailboxes
            .iter()
            .filter(|candidate| candidate.catalogue_coverage != "complete")
            .count();
        let mut details = Vec::new();
        if !progress.complete {
            details.push(format!(
                "Local search is still indexing: {} of {} messages.",
                progress.indexed_messages, progress.total_messages
            ));
        }
        if mailboxes.is_empty() {
            details.push(
                "Local mailbox catalogue has not been discovered for this account yet.".into(),
            );
        }
        if incomplete_mailboxes > 0 {
            details.push(format!(
                "Local catalogue is partial in {incomplete_mailboxes} of {} selectable mailboxes.",
                scoped_mailboxes.len()
            ));
        }
        coverage.push(SearchCoverage {
            account_id,
            mailbox: mailbox.clone(),
            state: SearchCoverageState::LocalCatalogue,
            detail: (!details.is_empty()).then(|| details.join(" ")),
        });
    }
    Ok(coverage)
}

fn local_body_index_search_coverage(
    account_id: Uuid,
    mailbox: Option<String>,
    coverage: &dakia_core::storage::LocalBodyIndexCoverage,
) -> Option<SearchCoverage> {
    (coverage.searchable_bodies < coverage.catalogue_messages).then(|| SearchCoverage {
        account_id,
        mailbox,
        state: SearchCoverageState::LocalBodyIndex,
        detail: Some(format!(
            "Local body search is partial: {} of {} messages have searchable text.",
            coverage.searchable_bodies, coverage.catalogue_messages
        )),
    })
}

#[cfg(test)]
fn next_local_search_cursor(
    local_capacity: usize,
    current: Option<dakia_core::MailCursor>,
    page: &MailConversationPage,
) -> Option<dakia_core::MailCursor> {
    if local_capacity == 0 {
        return current;
    }
    page.next_cursor.clone().or_else(|| {
        page.conversations
            .last()
            .map(|conversation| dakia_core::MailCursor {
                received_at: conversation.latest.received_at,
                id: conversation.latest.id.clone(),
            })
    })
}

/// A provider failure is terminal for that account in this submitted search.
/// Retrying the identical unavailable/auth-failed account on every local page
/// would create an endless continuation once local rows are exhausted. Other
/// accounts retain their own progress and may continue normally.
fn finish_failed_provider_account(
    sessions: &SearchSessionRegistry,
    session: &SearchSession,
    account_id: Uuid,
) {
    if let Some((cursor, _)) = sessions.provider_progress(session, account_id) {
        let _ = sessions.set_provider_progress(session, account_id, cursor, true);
    }
}

fn provider_search_has_pending_pages(
    request: &SearchRequestV2,
    sessions: &SearchSessionRegistry,
    session: &SearchSession,
    account_ids: &[Uuid],
) -> bool {
    matches!(request.execution_mode, SearchExecutionMode::Hybrid)
        && account_ids.iter().copied().any(|account_id| {
            sessions
                .provider_progress(session, account_id)
                .is_some_and(|(_, exhausted)| !exhausted)
        })
        || sessions.has_pending_provider_messages(session)
}

/// Split one public page's provider candidate budget across accounts in a
/// rotating order. The total is exactly bounded by `page_size`; when there
/// are more accounts than slots, later rounds begin at a different account.
fn fair_provider_round_budgets(
    account_ids: &[Uuid],
    page_size: usize,
    start: usize,
) -> Vec<(Uuid, usize)> {
    if account_ids.is_empty() || page_size == 0 {
        return Vec::new();
    }
    let count = account_ids.len();
    let base = page_size / count;
    let remainder = page_size % count;
    (0..count)
        .filter_map(|offset| {
            let account_id = account_ids[(start + offset) % count];
            let budget = base + usize::from(offset < remainder);
            (budget > 0).then_some((account_id, budget))
        })
        .collect()
}

/// Provider workers finish by account and mailbox availability, not message
/// date. Merge their persisted conversations with local matches using the
/// same newest-first key users see in every mailbox, with stable account and
/// conversation tie-breakers for equal provider timestamps.
fn sort_hybrid_conversations_newest_first(conversations: &mut [MailConversation]) {
    conversations.sort_by(|left, right| {
        right
            .latest
            .received_at
            .cmp(&left.latest.received_at)
            .then_with(|| right.latest.id.cmp(&left.latest.id))
            .then_with(|| left.account_id.cmp(&right.account_id))
            .then_with(|| left.id.cmp(&right.id))
    });
}

async fn search_v2_page(
    app: &tauri::AppHandle,
    state: Arc<AppState>,
    request: SearchRequestV2,
    session: SearchSession,
    continuation: Option<SearchContinuationV2>,
) -> Result<SearchPageV2, SearchErrorV2> {
    let expression = parse_search_query(&request.raw_query).map_err(SearchErrorV2::from)?;
    if !state.search_sessions.is_current(&session) {
        return Err(SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Transient,
            unsupported_operator: None,
            message: "This search was cancelled.".into(),
        });
    }
    let accounts = state.store.accounts().await.map_err(search_v2_error)?;
    let account_ids = enabled_search_account_ids(&accounts, &request.account_ids);
    // An empty explicit account selection means none of those requested
    // accounts can run, not "all accounts". Storage treats an empty list as
    // unscoped for legacy callers, so stop here before it can widen a
    // disabled-only saved search into another account's mail.
    if explicit_search_scope_has_no_enabled_accounts(&request.account_ids, &account_ids) {
        let mut seen = HashSet::new();
        let coverage = request
            .account_ids
            .iter()
            .copied()
            .filter(|account_id| seen.insert(*account_id))
            .map(|account_id| SearchCoverage {
                account_id,
                mailbox: request.scope.mailbox.clone(),
                state: SearchCoverageState::Unsupported,
                detail: Some("This selected account is disabled or unavailable.".into()),
            })
            .collect::<Vec<_>>();
        state.search_sessions.complete(&session);
        return Ok(SearchPageV2 {
            conversations: Vec::new(),
            match_evidence: Default::default(),
            coverage,
            continuation: None,
            session_id: session.session_id,
            revision: session.revision,
        });
    }
    let effective_mailbox = effective_search_scope_mailbox(&request, &expression);
    let mut query = search_v2_query(&request, account_ids.clone(), &expression);
    // V2 resumes from the SQL candidate keyset. Keep the old result cursor
    // only in the opaque continuation for legacy compatibility; applying it
    // here would make an older matching reply reintroduce a conversation that
    // was already emitted on an earlier page.
    query.cursor = None;
    let migration_progress = state
        .store
        .search_catalogue_v2_backfill_progress()
        .await
        .map_err(search_v2_error)?;
    let mut coverage = local_search_coverage(
        &state.store,
        &account_ids,
        effective_mailbox.clone(),
        &migration_progress,
    )
    .await
    .map_err(search_v2_error)?;
    // A complete v2 schema only means durable headers have been indexed. Body
    // text remains partial until each message has either a search-only fetch,
    // complete reader cache, or starred body cache. Report that distinction on
    // every page instead of claiming locally complete body matching.
    for account_id in &account_ids {
        let body_coverage = state
            .store
            .local_body_index_coverage(*account_id)
            .await
            .map_err(search_v2_error)?;
        if let Some(entry) =
            local_body_index_search_coverage(*account_id, effective_mailbox.clone(), &body_coverage)
        {
            coverage.push(entry);
        }
    }
    emit_search_progress(app, &state.search_sessions, &session, coverage.clone());
    let mut provider_messages = Vec::new();

    // Do not fetch another provider round while the previous round still has
    // queued candidates. That queue is bounded to one public page below, so a
    // high-match account cannot grow it on every continuation.
    let should_fetch_provider_round = matches!(request.execution_mode, SearchExecutionMode::Hybrid)
        && !state
            .search_sessions
            .has_pending_provider_messages(&session);
    if should_fetch_provider_round {
        let active_provider_accounts = account_ids
            .iter()
            .copied()
            .filter(|account_id| {
                state
                    .search_sessions
                    .provider_progress(&session, *account_id)
                    .is_some_and(|(_, exhausted)| !exhausted)
            })
            .collect::<Vec<_>>();
        let page_size = request.effective_page_size() as usize;
        let provider_round = state
            .search_sessions
            .next_provider_round_offset(&session, active_provider_accounts.len(), page_size)
            .map(|start| fair_provider_round_budgets(&active_provider_accounts, page_size, start))
            .unwrap_or_default();
        if !provider_round.is_empty() {
            let shared_state = state.clone();
            let shared_expression = expression.clone();
            let mailbox = effective_mailbox.clone();
            let include_spam_trash = request.scope.include_spam_trash;
            let provider_session = session.clone();
            let progress_app = app.clone();
            let results = run_bounded_ordered(
                provider_round,
                REMOTE_SEARCH_CONCURRENCY,
                state.remote_search_slots.clone(),
                move |(account_id, provider_page_size)| {
                    let state = shared_state.clone();
                    let expression = shared_expression.clone();
                    let mailbox = mailbox.clone();
                    let session = provider_session.clone();
                    let include_spam_trash = include_spam_trash;
                    let progress_app = progress_app.clone();
                    async move {
                        // Provider search uses its own bounded lane plus the
                        // shared connection budget. It deliberately does not
                        // hold the account mutation lock for the entire
                        // multi-command IMAP page: send/open operations use
                        // that lock and must take priority over a search.
                        let _remote = state
                            .remote_operation_slots
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("shared operation limiter must remain open");
                        if !state.search_sessions.is_current(&session) {
                            return Err((account_id, "cancelled".to_owned()));
                        }
                        let account = match enabled_account_for_operation(&state, account_id).await
                        {
                            Ok(account) => account,
                            Err(detail) => {
                                emit_search_progress(
                                    &progress_app,
                                    &state.search_sessions,
                                    &session,
                                    vec![provider_coverage_for_error(account_id, &detail)],
                                );
                                return Err((account_id, detail));
                            }
                        };
                        let publication_generation = state
                            .store
                            .account_search_generation(account_id)
                            .await
                            .map_err(|error| (account_id, error.to_string()))?;
                        let (cursor, exhausted) = state
                            .search_sessions
                            .provider_progress(&session, account_id)
                            .ok_or_else(|| (account_id, "cancelled".to_owned()))?;
                        if exhausted {
                            return Ok::<_, (Uuid, String)>((account_id, None));
                        }
                        let page = match MailService::new(state.store.clone())
                            .search_remote_expression_page_with_generation(
                                &account,
                                &expression,
                                mailbox.as_deref(),
                                &cursor,
                                provider_page_size,
                                include_spam_trash,
                                Some(&session),
                                Some(publication_generation),
                            )
                            .await
                        {
                            Ok(page) => page,
                            Err(error) => {
                                let detail = error.to_string();
                                emit_search_progress(
                                    &progress_app,
                                    &state.search_sessions,
                                    &session,
                                    vec![provider_coverage_for_error(account_id, &detail)],
                                );
                                return Err((account_id, detail));
                            }
                        };
                        // Re-check after provider work so an account removed or
                        // disabled mid-search cannot publish stale candidates.
                        if let Err(detail) = enabled_account_for_operation(&state, account_id).await
                        {
                            emit_search_progress(
                                &progress_app,
                                &state.search_sessions,
                                &session,
                                vec![provider_coverage_for_error(account_id, &detail)],
                            );
                            return Err((account_id, detail));
                        }
                        if !state.search_sessions.is_current(&session) {
                            return Err((account_id, "cancelled".to_owned()));
                        }
                        if !state.search_sessions.set_provider_progress(
                            &session,
                            account_id,
                            page.cursor.clone(),
                            page.exhausted,
                        ) {
                            return Err((account_id, "cancelled".to_owned()));
                        }
                        let live_coverage = page
                            .coverage
                            .iter()
                            .cloned()
                            .map(|mailbox| {
                                provider_mailbox_coverage(
                                    account_id,
                                    mailbox.mailbox,
                                    mailbox.state,
                                )
                            })
                            .collect::<Vec<_>>();
                        emit_search_progress(
                            &progress_app,
                            &state.search_sessions,
                            &session,
                            live_coverage,
                        );
                        Ok::<_, (Uuid, String)>((account_id, Some((page.coverage, page.messages))))
                    }
                },
            )
            .await;
            for result in results {
                match result {
                    Ok((_, None)) => {}
                    Ok((account_id, Some((mailboxes, messages)))) => {
                        provider_messages.extend(messages);
                        for mailbox in mailboxes {
                            coverage.push(provider_mailbox_coverage(
                                account_id,
                                mailbox.mailbox,
                                mailbox.state,
                            ));
                        }
                    }
                    Err((account_id, detail)) if detail == "cancelled" => {
                        coverage.push(SearchCoverage {
                            account_id,
                            mailbox: effective_mailbox.clone(),
                            state: SearchCoverageState::Cancelled,
                            detail: None,
                        })
                    }
                    Err((account_id, detail)) => {
                        finish_failed_provider_account(
                            &state.search_sessions,
                            &session,
                            account_id,
                        );
                        coverage.push(provider_coverage_for_error(account_id, &detail))
                    }
                }
            }
        }
    }
    if !state.search_sessions.enqueue_provider_message_ids_bounded(
        &session,
        provider_messages.into_iter().map(|message| message.id),
        request.effective_page_size() as usize,
    ) {
        return Err(search_v2_error("This search was cancelled."));
    }
    if !state.search_sessions.is_current(&session) {
        return Err(SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Transient,
            unsupported_operator: None,
            message: "This search was cancelled.".into(),
        });
    }
    // Provider pages are keyed by UID, while local pages are ordered by date.
    // Resolve freshly found provider matches directly into this session page
    // before applying the local date cursor, so a UID-old/date-new match can
    // never be filtered out forever by an earlier local continuation.
    let mut provider_conversations = Vec::new();
    let mut provider_conversation_ids = HashSet::new();
    let page_size = request.effective_page_size() as usize;
    for message_id in state
        .search_sessions
        .take_provider_message_ids(&session, page_size)
    {
        let Some(message) = state
            .store
            .message(&message_id)
            .await
            .map_err(search_v2_error)?
        else {
            continue;
        };
        let target = ConversationTarget {
            account_id: message.account_id.parse().map_err(search_v2_error)?,
            local_message_id: Some(message.id.clone()),
            rfc_message_id: None,
            thread_id: None,
            mailbox: None,
        };
        if let Some(conversation) = state
            .store
            .conversation_for_target(&target)
            .await
            .map_err(search_v2_error)?
        {
            if provider_conversation_ids.insert(conversation.id.clone())
                && state
                    .search_sessions
                    .mark_conversation_emitted(&session, &conversation.id)
            {
                provider_conversations.push(conversation);
            }
        }
    }
    let provider_keys = provider_conversations
        .iter()
        .map(|conversation| {
            (
                conversation.account_id.clone(),
                conversation.thread_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    // Ask storage to evaluate the exact provider conversations against the
    // durable search corpus. This uses cached/search-only body text and
    // attachment metadata, never the intentionally blank list hydration.
    let mut evidence_query = query.clone();
    evidence_query.cursor = None;
    let provider_evidence = state
        .store
        .search_match_evidence_for_conversations(&evidence_query, &provider_keys)
        .await
        .map_err(search_v2_error)?;
    let local_capacity = page_size.saturating_sub(provider_conversations.len());
    let page = if local_capacity == 0 {
        MailConversationPage {
            conversations: Vec::new(),
            next_cursor: query.cursor.clone(),
            match_evidence: Default::default(),
            candidate_cursor: continuation
                .as_ref()
                .and_then(|cursor| cursor.local_candidate_cursor.clone()),
            candidate_exhausted: false,
        }
    } else {
        query.limit = Some(local_capacity as u32);
        state
            .store
            .search_conversation_page_from_candidate(
                &query,
                continuation
                    .as_ref()
                    .and_then(|cursor| cursor.local_candidate_cursor.as_ref()),
                &state.search_sessions.emitted_conversation_ids(&session),
            )
            .await
            .map_err(search_v2_error)?
    };
    // V2 local pagination is driven exclusively by the candidate keyset
    // below. Retaining a legacy date cursor here would create a redundant
    // empty continuation after the final bounded local page, and would be
    // unsafe for an older matching reply in an already-emitted conversation.
    let next_local_cursor = None;
    let mut conversations = provider_conversations;
    conversations.extend(
        page.conversations
            .into_iter()
            .filter(|conversation| !provider_conversation_ids.contains(&conversation.id))
            .filter(|conversation| {
                state
                    .search_sessions
                    .mark_conversation_emitted(&session, &conversation.id)
            }),
    );
    sort_hybrid_conversations_newest_first(&mut conversations);
    let provider_pending =
        provider_search_has_pending_pages(&request, &state.search_sessions, &session, &account_ids);
    let next_offset = continuation
        .as_ref()
        .map(|cursor| cursor.offset)
        .unwrap_or_default()
        .saturating_add(conversations.len() as u64);
    let next_local_candidate_cursor = page.candidate_cursor.clone();
    let continuation =
        (next_local_cursor.is_some() || next_local_candidate_cursor.is_some() || provider_pending)
            .then(|| {
                SearchContinuationV2::new(&session, &request, next_offset)
                    .with_local_cursor(next_local_cursor)
                    .with_local_candidate_cursor(next_local_candidate_cursor)
                    .encode()
            });
    let mut match_evidence = page.match_evidence;
    match_evidence.extend(provider_evidence);
    match_evidence.retain(|conversation_id, _| {
        conversations
            .iter()
            .any(|conversation| conversation.id == *conversation_id)
    });
    if continuation.is_none() {
        state.search_sessions.complete(&session);
    }
    Ok(SearchPageV2 {
        conversations,
        match_evidence,
        coverage,
        continuation,
        session_id: session.session_id,
        revision: session.revision,
    })
}

#[tauri::command]
async fn start_search(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    request: SearchRequestV2,
) -> Result<SearchPageV2, SearchErrorV2> {
    if request.continuation.is_some() {
        return Err(SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Parse,
            unsupported_operator: None,
            message: "Use the next-page command for a search continuation.".into(),
        });
    }
    parse_search_query(&request.raw_query).map_err(SearchErrorV2::from)?;
    let app_state = state.inner().clone();
    let session = app_state.search_sessions.begin(&request);
    if session.is_cancelled() {
        // A client-generated ID may have been cancelled before this command
        // won the IPC race. Remove the one-shot cancelled session immediately
        // rather than retaining it as an inactive registry entry.
        app_state.search_sessions.cancel(session.session_id);
        return Err(SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Transient,
            unsupported_operator: None,
            message: "This search was cancelled.".into(),
        });
    }
    search_v2_page(&app, app_state, request, session, None).await
}

#[tauri::command]
async fn next_search_page(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    request: SearchRequestV2,
) -> Result<SearchPageV2, SearchErrorV2> {
    let continuation = request
        .decode_continuation()
        .map_err(|_| SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Parse,
            unsupported_operator: None,
            message: "This search page is no longer valid.".into(),
        })?
        .ok_or_else(|| SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Parse,
            unsupported_operator: None,
            message: "A search continuation is required.".into(),
        })?;
    let app_state = state.inner().clone();
    let session = app_state
        .search_sessions
        .current_for(continuation.session_id, &request)
        .ok_or_else(|| SearchErrorV2 {
            position: None,
            category: SearchErrorCategory::Transient,
            unsupported_operator: None,
            message: "This search was superseded or cancelled.".into(),
        })?;
    search_v2_page(&app, app_state, request, session, Some(continuation)).await
}

#[tauri::command]
async fn cancel_search(state: State<'_, Arc<AppState>>, session_id: Uuid) -> Result<(), String> {
    state.search_sessions.cancel(session_id);
    Ok(())
}

#[cfg(test)]
mod search_v2_command_tests {
    use super::*;

    fn request() -> SearchRequestV2 {
        SearchRequestV2 {
            raw_query: "from:person@example.test".into(),
            client_request_id: None,
            account_ids: vec![Uuid::from_u128(7)],
            scope: dakia_core::SearchScopeV2 {
                mailbox: None,
                include_spam_trash: false,
            },
            execution_mode: SearchExecutionMode::Hybrid,
            page_size: 50,
            continuation: None,
        }
    }

    fn account_for_search_scope(email: &str) -> Account {
        AccountDraft {
            email: email.into(),
            display_name: email.into(),
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
        .into_account(provider::by_id("fastmail").expect("Fastmail preset"))
    }

    fn scoped_search_message(account_id: Uuid, id: &str) -> MailSummary {
        MailSummary {
            id: id.into(),
            account_id: account_id.to_string(),
            mailbox: "Inbox".into(),
            uid: 1,
            message_id: Some(format!("<{id}@example.test>")),
            in_reply_to: None,
            reference_ids: None,
            thread_id: format!("thread-{id}"),
            subject: "account scope needle".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: chrono::Utc::now(),
            snippet: "account scope needle".into(),
            body_text: String::new(),
            body_html: None,
            content_state: "headers_only".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: true,
            is_flagged: false,
            is_answered: false,
            is_draft: false,
            has_attachments: false,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: Vec::new(),
        }
    }

    fn search_conversation(
        account_id: Uuid,
        id: &str,
        received_at: chrono::DateTime<chrono::Utc>,
    ) -> MailConversation {
        let mut latest = scoped_search_message(account_id, id);
        latest.received_at = received_at;
        MailConversation {
            id: format!("conversation-{id}"),
            account_id: account_id.to_string(),
            thread_id: latest.thread_id.clone(),
            messages: vec![latest.clone()],
            source_messages: vec![latest.clone()],
            latest,
            message_count: 1,
            unread: false,
            has_attachments: false,
            participants: Vec::new(),
        }
    }

    #[test]
    fn hybrid_results_sort_newest_first_across_local_and_provider_accounts() {
        let base = "2026-09-12T10:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("timestamp");
        let mut results = vec![
            // Simulate completion order: a remote older mailbox, a local
            // result, then another account's newer remote result.
            search_conversation(Uuid::from_u128(1), "provider-old", base),
            search_conversation(
                Uuid::from_u128(2),
                "local-middle",
                base + chrono::Duration::minutes(5),
            ),
            search_conversation(
                Uuid::from_u128(3),
                "provider-new",
                base + chrono::Duration::minutes(10),
            ),
        ];
        sort_hybrid_conversations_newest_first(&mut results);
        assert_eq!(
            results
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "conversation-provider-new",
                "conversation-local-middle",
                "conversation-provider-old",
            ]
        );
    }

    #[test]
    fn search_mailbox_catalogue_is_stably_sorted_and_deduplicated_per_account_path() {
        let mailbox =
            |account_id: &str, remote_path: &str, local_path: &str, id: &str| SelectableMailbox {
                id: id.into(),
                account_id: account_id.into(),
                remote_path: remote_path.into(),
                local_path: local_path.into(),
                hierarchy_delimiter: Some("/".into()),
                parent_id: None,
                parent_path: None,
                special_use: None,
                selectable: true,
                uid_validity: None,
                catalogue_coverage: "unknown".into(),
            };
        let listed = sort_and_deduplicate_search_mailboxes(vec![
            mailbox("account-b", "Archive", "Archive", "b-archive"),
            mailbox("account-a", "Projects", "Projects", "a-projects"),
            mailbox("account-a", "Projects", "Projects duplicate", "a-duplicate"),
            mailbox("account-a", "INBOX", "INBOX", "a-inbox"),
        ]);
        assert_eq!(
            listed
                .iter()
                .map(|entry| (entry.account_id.as_str(), entry.remote_path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("account-a", "INBOX"),
                ("account-a", "Projects"),
                ("account-b", "Archive"),
            ]
        );
    }

    #[tokio::test]
    async fn disabled_requested_accounts_are_excluded_from_v2_local_results_and_coverage() {
        let store = Store::in_memory().await.expect("in-memory store");
        let enabled = account_for_search_scope("enabled-search@example.test");
        let mut disabled = account_for_search_scope("disabled-search@example.test");
        disabled.enabled = false;
        store
            .save_account(&enabled)
            .await
            .expect("save enabled account");
        store
            .save_account(&disabled)
            .await
            .expect("save disabled account");
        store
            .upsert_messages(&[
                scoped_search_message(enabled.id, "enabled-search-message"),
                scoped_search_message(disabled.id, "disabled-search-message"),
            ])
            .await
            .expect("save local catalogue");

        let requested = vec![disabled.id, enabled.id];
        let account_ids =
            enabled_search_account_ids(&store.accounts().await.expect("read accounts"), &requested);
        assert_eq!(account_ids, vec![enabled.id]);
        assert!(!explicit_search_scope_has_no_enabled_accounts(
            &requested,
            &account_ids
        ));
        let disabled_only = vec![disabled.id];
        let no_enabled = enabled_search_account_ids(
            &store.accounts().await.expect("read accounts"),
            &disabled_only,
        );
        assert!(no_enabled.is_empty());
        assert!(explicit_search_scope_has_no_enabled_accounts(
            &disabled_only,
            &no_enabled
        ));

        let page = store
            .search_conversation_page(&SearchQuery {
                text: "account scope needle".into(),
                account_ids: account_ids.clone(),
                limit: Some(20),
                ..SearchQuery::default()
            })
            .await
            .expect("search enabled local catalogue");
        assert_eq!(page.conversations.len(), 1);
        assert_eq!(page.conversations[0].account_id, enabled.id.to_string());

        let coverage = local_search_coverage(
            &store,
            &account_ids,
            None,
            &dakia_core::storage::SearchCatalogueV2BackfillProgress {
                indexed_messages: 2,
                total_messages: 2,
                complete: true,
            },
        )
        .await
        .expect("read mailbox catalogue coverage");
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].account_id, enabled.id);
        // `search_v2_page` uses the same filtered list for its provider work,
        // so a disabled account cannot be contacted or receive coverage.
    }

    #[test]
    fn legacy_search_never_widens_a_disabled_or_missing_explicit_scope() {
        let enabled = account_for_search_scope("legacy-enabled@example.test");
        let mut disabled = account_for_search_scope("legacy-disabled@example.test");
        disabled.enabled = false;
        let accounts = vec![enabled.clone(), disabled.clone()];
        let base = SearchQuery {
            text: "needle".into(),
            account_ids: vec![disabled.id],
            ..SearchQuery::default()
        };
        assert!(legacy_search_query_for_enabled_accounts(base, &accounts).is_none());

        let missing = SearchQuery {
            text: "needle".into(),
            account_ids: vec![Uuid::from_u128(99_001)],
            ..SearchQuery::default()
        };
        assert!(legacy_search_query_for_enabled_accounts(missing, &accounts).is_none());

        let mixed = SearchQuery {
            text: "needle".into(),
            account_ids: vec![disabled.id, enabled.id],
            ..SearchQuery::default()
        };
        assert_eq!(
            legacy_search_query_for_enabled_accounts(mixed, &accounts)
                .expect("mixed scope keeps the active account")
                .account_ids,
            vec![enabled.id]
        );
    }

    #[test]
    fn failed_provider_account_does_not_leave_an_endless_v2_continuation() {
        let sessions = SearchSessionRegistry::default();
        let offline_account = Uuid::from_u128(801);
        let healthy_account = Uuid::from_u128(802);
        let mut request = request();
        request.account_ids = vec![offline_account];
        let session = sessions.begin(&request);

        // This models an offline/auth error after no local or provider rows
        // were found. It must not keep the public continuation alive.
        finish_failed_provider_account(&sessions, &session, offline_account);
        assert!(sessions
            .provider_progress(&session, offline_account)
            .is_some_and(|(_, exhausted)| exhausted));
        assert!(!provider_search_has_pending_pages(
            &request,
            &sessions,
            &session,
            &request.account_ids,
        ));

        // A failure in one account does not suppress a different account's
        // still-pending provider work.
        let mut mixed_request = request.clone();
        mixed_request.account_ids = vec![offline_account, healthy_account];
        let mixed_session = sessions.begin(&mixed_request);
        finish_failed_provider_account(&sessions, &mixed_session, offline_account);
        assert!(provider_search_has_pending_pages(
            &mixed_request,
            &sessions,
            &mixed_session,
            &mixed_request.account_ids,
        ));
        let (cursor, _) = sessions
            .provider_progress(&mixed_session, healthy_account)
            .expect("active healthy account has fresh provider progress");
        assert!(sessions.set_provider_progress(&mixed_session, healthy_account, cursor, true));
        assert!(!provider_search_has_pending_pages(
            &mixed_request,
            &sessions,
            &mixed_session,
            &mixed_request.account_ids,
        ));
    }

    #[test]
    fn provider_round_queue_is_page_bounded_and_fair_across_high_match_accounts() {
        let accounts = [
            Uuid::from_u128(901),
            Uuid::from_u128(902),
            Uuid::from_u128(903),
        ];
        let first_round = fair_provider_round_budgets(&accounts, 5, 0);
        assert_eq!(
            first_round,
            vec![(accounts[0], 2), (accounts[1], 2), (accounts[2], 1)]
        );
        assert_eq!(
            first_round.iter().map(|(_, budget)| budget).sum::<usize>(),
            5,
            "one provider round can never queue more candidates than its public page"
        );

        let sessions = SearchSessionRegistry::default();
        let mut request = request();
        request.account_ids = accounts.to_vec();
        request.page_size = 5;
        let session = sessions.begin(&request);
        let ids = first_round
            .iter()
            .flat_map(|(account_id, budget)| {
                (0..*budget).map(move |index| format!("{account_id}-{index}"))
            })
            .collect::<Vec<_>>();
        assert!(sessions.enqueue_provider_message_ids_bounded(&session, ids.clone(), 5));
        assert!(sessions.has_pending_provider_messages(&session));
        assert_eq!(sessions.take_provider_message_ids(&session, 10), ids);
        assert!(!sessions.has_pending_provider_messages(&session));

        let first_start = sessions
            .next_provider_round_offset(&session, accounts.len(), 5)
            .expect("active session");
        let second_start = sessions
            .next_provider_round_offset(&session, accounts.len(), 5)
            .expect("active session");
        assert_eq!(first_start, 0);
        assert_eq!(second_start, 2, "a later round starts with another account");
    }

    #[test]
    fn v2_query_keeps_the_raw_query_and_explicitly_widens_spam_trash() {
        let account_id = Uuid::from_u128(7);
        let expression = parse_search_query(&request().raw_query).unwrap();
        let normal = search_v2_query(&request(), vec![account_id], &expression);
        assert_eq!(normal.text, "from:person@example.test");
        assert_eq!(normal.account_ids, vec![account_id]);

        let mut widened_request = request();
        widened_request.scope.include_spam_trash = true;
        let widened_expression = parse_search_query(&widened_request.raw_query).unwrap();
        let widened = search_v2_query(&widened_request, vec![account_id], &widened_expression);
        assert_eq!(widened.text, "(from:person@example.test) in:*");
        assert!(parse_search_query(&widened.text).is_ok());
    }

    #[test]
    fn explicit_in_syntax_replaces_the_ambient_folder_for_local_and_provider_planning() {
        let account_id = Uuid::from_u128(7);
        let mut request = request();
        request.scope.mailbox = Some("Inbox".into());
        request.raw_query = "in:Sent from:person@example.test".into();
        let expression = parse_search_query(&request.raw_query).unwrap();
        assert!(expression_has_folder_predicate(&expression));
        assert_eq!(effective_search_scope_mailbox(&request, &expression), None);
        assert_eq!(
            search_v2_query(&request, vec![account_id], &expression).mailbox,
            None,
            "local SQL must not intersect Inbox with an explicit Sent predicate"
        );

        for query in ["in:*", "in:Spam", "in:Projects/*"] {
            request.raw_query = query.into();
            let expression = parse_search_query(&request.raw_query).unwrap();
            assert_eq!(
                effective_search_scope_mailbox(&request, &expression),
                None,
                "{query}"
            );
        }
    }

    #[test]
    fn provider_failures_have_safe_per_account_coverage() {
        let coverage = provider_coverage_for_error(Uuid::nil(), "mailbox identity changed");
        assert_eq!(coverage.state, SearchCoverageState::MailboxChanged);
        assert_eq!(
            coverage.detail.as_deref(),
            Some("Provider search was unavailable for this account.")
        );
    }

    #[test]
    fn transient_search_errors_do_not_expose_provider_or_sqlite_details() {
        let error = search_v2_error(
            "SQLite error near SELECT for imap.example.test: credentials are unavailable",
        );
        assert_eq!(error.category, SearchErrorCategory::Transient);
        assert_eq!(
            error.message,
            "Search could not be completed. Please try again."
        );
        assert!(!error.message.contains("SQLite"));
        assert!(!error.message.contains("imap.example.test"));
    }

    #[test]
    fn search_progress_payload_is_bound_to_the_current_session_revision() {
        let update = SearchProgressUpdate {
            session_id: Uuid::from_u128(54),
            revision: 8,
            coverage: vec![SearchCoverage {
                account_id: Uuid::from_u128(9),
                mailbox: Some("Inbox".into()),
                state: SearchCoverageState::ProviderSearched,
                detail: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(update).unwrap(),
            serde_json::json!({
                "sessionId": "00000000-0000-0000-0000-000000000036",
                "revision": 8,
                "coverage": [{
                    "account_id": "00000000-0000-0000-0000-000000000009",
                    "mailbox": "Inbox",
                    "state": "provider_searched",
                    "detail": null,
                }]
            })
        );
    }

    #[test]
    fn provider_coverage_keeps_a_successful_folder_when_another_folder_fails() {
        let account_id = Uuid::from_u128(9);
        let inbox = provider_mailbox_coverage(
            account_id,
            "Inbox".into(),
            ProviderMailboxSearchState::Searched,
        );
        let archive = provider_mailbox_coverage(
            account_id,
            "Archive".into(),
            ProviderMailboxSearchState::Offline,
        );
        assert_eq!(inbox.state, SearchCoverageState::ProviderSearched);
        assert_eq!(inbox.mailbox.as_deref(), Some("Inbox"));
        assert_eq!(archive.state, SearchCoverageState::Offline);
        assert_eq!(archive.mailbox.as_deref(), Some("Archive"));
    }

    #[test]
    fn provider_partial_and_search_body_cache_coverage_have_distinct_meanings() {
        let account_id = Uuid::from_u128(9);
        let current_candidate = provider_mailbox_coverage(
            account_id,
            "Inbox".into(),
            ProviderMailboxSearchState::Partial,
        );
        assert_eq!(
            current_candidate.state,
            SearchCoverageState::ProviderPartial
        );
        assert_eq!(
            current_candidate.detail.as_deref(),
            Some("Some provider candidates could not be fully verified for this mailbox.")
        );
        let later_local_page = provider_mailbox_coverage(
            account_id,
            "Inbox".into(),
            ProviderMailboxSearchState::SearchBodyCacheIncomplete,
        );
        assert_eq!(later_local_page.state, SearchCoverageState::LocalBodyIndex);
        assert_eq!(
            later_local_page.detail.as_deref(),
            Some("Provider results were found, but some text is not available for later local pages.")
        );
    }

    #[tokio::test]
    async fn incomplete_catalogue_backfill_and_partial_account_mailboxes_are_visible_in_v2_coverage(
    ) {
        let store = Store::in_memory().await.unwrap();
        let account = account_for_search_scope("partial-catalogue@example.test");
        let account_id = account.id;
        store.save_account(&account).await.unwrap();
        store
            .upsert_selectable_mailbox(
                account_id,
                &SelectableMailboxDraft {
                    remote_path: "Projects".into(),
                    local_path: Some("Projects".into()),
                    hierarchy_delimiter: Some("/".into()),
                    parent_id: None,
                    parent_path: None,
                    special_use: None,
                    selectable: true,
                    uid_validity: Some(42),
                    catalogue_coverage: "partial".into(),
                },
            )
            .await
            .unwrap();
        let progress = dakia_core::storage::SearchCatalogueV2BackfillProgress {
            indexed_messages: 500,
            total_messages: 1_200,
            complete: false,
        };
        let coverage = local_search_coverage(&store, &[account_id], None, &progress)
            .await
            .unwrap();
        assert!(coverage.iter().any(|entry| {
            entry.state == SearchCoverageState::LocalCatalogue
                && entry.detail.as_deref().is_some_and(|detail| {
                    detail.contains("Local search is still indexing: 500 of 1200 messages.")
                })
                && entry.detail.as_deref().is_some_and(|detail| {
                    detail.contains("Local catalogue is partial in 1 of 1 selectable mailboxes.")
                })
        }));
        assert!(!coverage
            .iter()
            .any(|entry| entry.state == SearchCoverageState::LocalBodyIndex));
    }

    #[tokio::test]
    async fn local_coverage_keeps_an_enabled_account_partial_until_its_mailbox_catalogue_exists() {
        let store = Store::in_memory().await.unwrap();
        let account = account_for_search_scope("undiscovered-mailboxes@example.test");
        store.save_account(&account).await.unwrap();
        let coverage = local_search_coverage(
            &store,
            &[account.id],
            None,
            &dakia_core::storage::SearchCatalogueV2BackfillProgress {
                indexed_messages: 0,
                total_messages: 0,
                complete: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].account_id, account.id);
        assert_eq!(
            coverage[0].detail.as_deref(),
            Some("Local mailbox catalogue has not been discovered for this account yet.")
        );
    }

    #[test]
    fn headers_only_messages_remain_visible_as_partial_body_coverage_after_migration() {
        let entry = local_body_index_search_coverage(
            Uuid::from_u128(11),
            Some("Inbox".into()),
            &dakia_core::storage::LocalBodyIndexCoverage {
                account_id: Uuid::from_u128(11).to_string(),
                catalogue_messages: 100,
                searchable_bodies: 25,
            },
        )
        .expect("headers-only corpus must not claim complete body coverage");
        assert_eq!(entry.state, SearchCoverageState::LocalBodyIndex);
        assert_eq!(entry.mailbox.as_deref(), Some("Inbox"));
        assert_eq!(
            entry.detail.as_deref(),
            Some("Local body search is partial: 25 of 100 messages have searchable text.")
        );
    }

    #[test]
    fn contacted_people_background_backfill_emits_only_when_people_changed() {
        let unchanged = dakia_core::storage::ContactedPeopleBackfillProgress {
            processed_messages: 100,
            changed_people: 0,
            complete: false,
        };
        let changed = dakia_core::storage::ContactedPeopleBackfillProgress {
            processed_messages: 1,
            changed_people: 1,
            complete: true,
        };
        assert!(!contacted_people_backfill_changed(&unchanged));
        assert!(contacted_people_backfill_changed(&changed));
    }

    #[test]
    fn provider_only_page_preserves_the_local_cursor_for_the_next_page() {
        let existing = dakia_core::MailCursor {
            received_at: chrono::Utc::now(),
            id: "local-before-provider-page".into(),
        };
        let provider_only_page = MailConversationPage {
            conversations: Vec::new(),
            match_evidence: Default::default(),
            next_cursor: Some(existing.clone()),
            candidate_cursor: None,
            candidate_exhausted: false,
        };
        assert_eq!(
            next_local_search_cursor(0, Some(existing.clone()), &provider_only_page),
            Some(existing),
            "a full provider page must not derive a cursor from provider dates or IDs"
        );
        assert_eq!(
            next_local_search_cursor(
                0,
                None,
                &MailConversationPage {
                    conversations: Vec::new(),
                    match_evidence: Default::default(),
                    next_cursor: None,
                    candidate_cursor: None,
                    candidate_exhausted: false,
                }
            ),
            None,
            "the first provider-only page leaves newer local-only matches eligible"
        );
    }
}

#[tauri::command]
async fn search_remote(
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<Vec<MailSummary>, String> {
    if query.text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let limit = query.limit.unwrap_or(100).min(500) as usize;
    let mut results = Vec::new();
    let search_state = state.inner().clone();
    let search_text = query.text.clone();
    let search_mailbox = query.mailbox.clone();
    let searches = run_bounded_ordered(
        query.account_ids.clone(),
        REMOTE_SEARCH_CONCURRENCY,
        state.remote_search_slots.clone(),
        move |account_id| {
            let state = search_state.clone();
            let text = search_text.clone();
            let mailbox = search_mailbox.clone();
            async move {
                let _remote = state
                    .remote_operation_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("shared operation limiter must remain open");
                let account = enabled_account_for_operation(&state, account_id).await?;
                let hits = MailService::new(state.store.clone())
                    .search_remote(&account, &text, mailbox.as_deref(), limit)
                    .await
                    .map_err(error)?;
                // Provider work must not publish a result after the account
                // was disabled or removed while its IMAP command was running.
                enabled_account_for_operation(&state, account_id).await?;
                Ok::<_, String>(hits)
            }
        },
    )
    .await;
    // Reassemble completion results in requested account order. This makes the
    // first returned failure deterministic and preserves stable tie ordering.
    for hits in searches {
        let hits = hits?;
        for message in hits {
            if query.mailbox.is_none()
                && matches!(message.mailbox.split("::").next(), Some("Spam" | "Trash"))
            {
                continue;
            }
            if (!query.unread_only || !message.is_read)
                && (!query.flagged_only || message.is_flagged)
                && (!query.unflagged_only || !message.is_flagged)
            {
                results.push(message);
            }
        }
    }
    results.sort_by_key(|result| std::cmp::Reverse(result.received_at));
    results.truncate(limit);
    Ok(results)
}

#[tauri::command]
async fn set_message_category(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    category: String,
) -> Result<(), String> {
    state
        .store
        .set_message_category(&message_id, &category)
        .await
        .map_err(error)
}

#[tauri::command]
async fn set_message_starred(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    starred: bool,
) -> Result<dakia_core::MailSummary, String> {
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    invalidate_searches_for_account(state.inner(), account_id).await?;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .set_flagged(&account, &message.mailbox, message.uid as u32, starred)
        .await
        .map_err(error)?;
    state
        .store
        .set_message_flagged(&message_id, starred)
        .await
        .map_err(error)?;
    if starred {
        match MailService::new(state.store.clone())
            .hydrate_message(&account, &message.mailbox, message.uid as u32)
            .await
        {
            Ok(hydrated) => Ok(hydrated),
            Err(_) => state
                .store
                .message(&message_id)
                .await
                .map_err(error)?
                .ok_or_else(|| "Message not found".to_owned()),
        }
    } else {
        state
            .store
            .message(&message_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Message not found".to_owned())
    }
}

#[tauri::command]
async fn set_message_read(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    read: bool,
) -> Result<(), String> {
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    invalidate_searches_for_account(state.inner(), account_id).await?;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .set_read(&account, &message.mailbox, message.uid as u32, read)
        .await
        .map_err(error)?;
    state
        .store
        .set_message_read(&message_id, read)
        .await
        .map_err(error)
}

#[tauri::command]
async fn starred_conversation_count(
    state: State<'_, Arc<AppState>>,
    account_ids: Vec<Uuid>,
) -> Result<u64, String> {
    state
        .store
        .starred_conversation_count(&account_ids)
        .await
        .map_err(error)
}

fn kick_classification(state: Arc<AppState>) -> u64 {
    let (generation, should_start) = state.classification.request();
    if should_start {
        tauri::async_runtime::spawn(async move {
            if let Err(error) = drain_pending_classifications(state).await {
                tracing::error!(error = %error, "background message classification failed");
            }
        });
    }
    generation
}

async fn classify_pending_batch(state: Arc<AppState>) -> anyhow::Result<usize> {
    let model_revision = state
        .classifier
        .lock()
        .map_err(|_| anyhow::anyhow!("email classifier lock is unavailable"))?
        .revision()
        .to_owned();
    state
        .store
        .claim_classification_revision(&state.classification_owner, &model_revision)
        .await?;
    let messages = state
        .store
        .messages_for_model_classification_batch(CLASSIFICATION_BATCH_SIZE)
        .await?;
    if messages.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = messages.iter().map(|message| message.id.clone()).collect();
    let known_correspondence = state
        .store
        .messages_from_known_correspondents(&messages)
        .await?;
    let inputs: Vec<EmailClassificationInput> = messages
        .iter()
        .map(|message| {
            let body = if message.body_text.trim().is_empty() {
                &message.snippet
            } else {
                &message.body_text
            };
            EmailClassificationInput::new(
                message.from_name.as_deref(),
                &message.from_address,
                &message.subject,
                body,
                &message.classification_signals,
            )
            .with_known_correspondence(known_correspondence.contains(&message.id))
        })
        .collect();
    let classifier_state = state.clone();
    let classifications = tauri::async_runtime::spawn_blocking(move || {
        let mut classifier = classifier_state
            .classifier
            .lock()
            .map_err(|_| anyhow::anyhow!("email classifier lock is unavailable"))?;
        classifier.classify(&inputs)
    })
    .await
    .map_err(|error| anyhow::anyhow!("email classifier task failed: {error}"))??;
    validate_classification_output_count(ids.len(), classifications.len())?;
    let updates: Vec<ModelClassificationUpdate> = messages
        .iter()
        .zip(classifications)
        .map(|(message, result)| {
            ModelClassificationUpdate::from_message(
                message,
                result.category,
                result.confidence,
                known_correspondence.contains(&message.id),
                &state.classification_owner,
                &model_revision,
            )
        })
        .collect();
    let count = updates.len();
    let applied = state.store.apply_model_classifications(&updates).await?;
    validate_classification_apply_count(count, applied)
}

async fn drain_pending_classifications(state: Arc<AppState>) -> anyhow::Result<()> {
    let result = drain_pending_classifications_owned(state.clone()).await;
    let release = state
        .store
        .release_classification_revision(&state.classification_owner)
        .await;
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn drain_pending_classifications_owned(state: Arc<AppState>) -> anyhow::Result<()> {
    let mut classified = 0;
    loop {
        let generation = state.classification.next_generation();
        match retry_classification_batch({
            let state = state.clone();
            move || classify_pending_batch(state.clone())
        })
        .await
        {
            Ok(count) => {
                classified += count;
                if count == CLASSIFICATION_BATCH_SIZE {
                    continue;
                }
                if !state
                    .classification
                    .finish_generation(generation, classified)
                {
                    return Ok(());
                }
            }
            Err(error) => {
                let failure = error.to_string();
                if state.classification.fail_generation(generation, failure) {
                    continue;
                }
                return Err(error);
            }
        }
    }
}

async fn classify_pending_messages(state: Arc<AppState>) -> anyhow::Result<usize> {
    let generation = kick_classification(state.clone());
    state.classification.wait_for(generation).await
}

#[tauri::command]
async fn classify_pending(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    classify_pending_messages(state.inner().clone())
        .await
        .map_err(error)
}

#[tauri::command]
async fn start_realtime_sync(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    state.realtime.reconcile(app).await.map_err(error)
}

#[tauri::command]
async fn reconcile_realtime_sync(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    state.realtime.reconcile(app).await.map_err(error)
}

#[tauri::command]
async fn realtime_sync_status(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<RealtimeSyncStatus>, String> {
    Ok(state.realtime.statuses().await)
}

#[tauri::command]
fn record_notification_delivered(
    account_id: Uuid,
    event_id: Uuid,
    detected_at: String,
) -> Result<(), String> {
    let detected_at = chrono::DateTime::parse_from_rfc3339(&detected_at).map_err(error)?;
    let latency_ms = chrono::Utc::now()
        .signed_duration_since(detected_at.with_timezone(&chrono::Utc))
        .num_milliseconds()
        .max(0);
    tracing::info!(
        account_id = %account_id,
        event_id = %event_id,
        notification_latency_ms = latency_ms,
        "new mail notification delivered"
    );
    Ok(())
}

#[tauri::command]
async fn hydrate_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<dakia_core::MailSummary, String> {
    let message = hydrated_message(state.inner(), &message_id).await?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    kick_classification(state.inner().clone());
    let _ = app.emit(
        "mail-hydrated",
        serde_json::json!({
            "accountId": account_id,
            "messageId": message.id,
        }),
    );
    Ok(message)
}

async fn hydrated_message(state: &Arc<AppState>, message_id: &str) -> Result<MailSummary, String> {
    let content = load_message_content(state, message_id).await?;
    let mut message = state
        .store
        .message(message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    message.body_text = content.body_text;
    message.body_html = content.body_html;
    message.unsubscribe_kind = content.unsubscribe_kind;
    message.content_state = "complete".into();
    message.attachments = content
        .attachments
        .into_iter()
        .map(|attachment| dakia_core::storage::AttachmentData {
            attachment,
            bytes: Vec::new(),
        })
        .collect();
    message.has_attachments = !message.attachments.is_empty();
    Ok(message)
}

fn publish_mail_rebuild_progress(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    account_id: Uuid,
    progress: SyncProgress,
) {
    let reset_before_sync = state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .get(&account_id)
        .map(|job| job.reset_before_sync)
        .unwrap_or(false);
    let update = MailRebuildProgress {
        account_id,
        phase: progress.phase.to_owned(),
        completed: progress.completed,
        total: progress.total,
        reset_before_sync,
    };
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account_id, update.clone());
    let _ = app.emit("mail-rebuild-progress", &update);
}

fn request_mail_rebuild_cancel(
    state: &Arc<AppState>,
    account_id: Uuid,
    disposition: MailRebuildCancellationDisposition,
) {
    state
        .mail_rebuild_cancellations
        .request(account_id, disposition);
}

fn reserve_mail_rebuild(state: &Arc<AppState>, account_id: Uuid) -> bool {
    let reserved = state
        .mail_rebuild_running
        .lock()
        .expect("mail rebuild reservation lock poisoned")
        .insert(account_id);
    if reserved {
        // Register the cancellation channel with the reservation, before the
        // spawned task can run. A namespace-changing update can now cancel a
        // queued worker rather than missing the pre-registration window.
        state.mail_rebuild_cancellations.reserve(account_id);
    }
    reserved
}

fn release_mail_rebuild(state: &Arc<AppState>, account_id: Uuid) {
    let released = state
        .mail_rebuild_running
        .lock()
        .expect("mail rebuild reservation lock poisoned")
        .remove(&account_id);
    if released {
        state
            .mail_rebuild_cancellations
            .release_reservation(account_id);
    }
}

async fn schedule_mail_rebuild(
    state: &Arc<AppState>,
    account_id: Uuid,
    reset_before_sync: bool,
) -> anyhow::Result<()> {
    let job = MailRebuildJob {
        account_id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync,
    };
    // Persistence comes first. A realtime reconcile must never win the race
    // with the durable replacement intent.
    state.store.save_mail_rebuild_job(&job).await?;
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account_id, job.into());
    Ok(())
}

fn reset_mail_rebuild_job(account_id: Uuid) -> MailRebuildJob {
    MailRebuildJob {
        account_id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync: true,
    }
}

async fn save_account_with_rebuild_intent(
    state: &Arc<AppState>,
    account: &Account,
    reset_before_sync: bool,
    previous_secret_name: Option<&str>,
    current_secret_name: &str,
) -> anyhow::Result<()> {
    if !reset_before_sync {
        return state.store.save_account(account).await;
    }
    let job = reset_mail_rebuild_job(account.id);
    // The namespace replacement is indivisible: a reconnect can never leave
    // a changed remote identity saved without its required reset job.
    if let Some(previous_secret_name) = previous_secret_name {
        state
            .store
            .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                account,
                &job,
                Some(previous_secret_name),
                current_secret_name,
            )
            .await?;
    } else {
        state
            .store
            .save_account_with_reset_mail_rebuild_job(account, &job)
            .await?;
    }
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account.id, job.into());
    Ok(())
}

async fn resume_scheduled_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
) {
    let in_memory = {
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .get(&account.id)
            .map(|job| job.reset_before_sync)
    };
    let reset_before_sync = match in_memory {
        Some(reset_before_sync) => Some(reset_before_sync),
        None => match state.store.mail_rebuild_jobs().await {
            Ok(jobs) => jobs
                .into_iter()
                .find(|job| job.account_id == account.id)
                .map(|job| job.reset_before_sync),
            Err(error) => {
                tracing::warn!(account_id = %account.id, error = %error, "could not load retained mail rebuild job");
                None
            }
        },
    };
    let Some(reset_before_sync) = reset_before_sync else {
        return;
    };
    if !reserve_mail_rebuild(&state, account.id) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_mail_rebuild(app, state, account, reset_before_sync).await {
            tracing::warn!(error = %error, "could not resume retained mail rebuild");
        }
    });
}

fn should_retain_mail_rebuild_job(
    error: &anyhow::Error,
    disposition: MailRebuildCancellationDisposition,
    reset_before_sync: bool,
) -> bool {
    if matches!(
        disposition,
        MailRebuildCancellationDisposition::Retain | MailRebuildCancellationDisposition::Replace
    ) {
        return true;
    }
    if matches!(disposition, MailRebuildCancellationDisposition::Remove)
        || error.to_string() == "mail rebuild cancelled"
    {
        return false;
    }
    // A first or replacement index has no trusted catalogue yet. Keep its
    // reset intent even for a credential error so reconnecting the same
    // namespace can finish the original replacement instead of silently
    // downgrading to a normal incremental sync.
    if reset_before_sync {
        return true;
    }
    !error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("imap authentication rejected")
            || message.contains("credentials are not stored")
            || message.contains("stored oauth credentials are invalid")
    })
}

fn effective_rebuild_reset(requested_reset: bool, durable_reset: Option<bool>) -> bool {
    requested_reset || durable_reset.unwrap_or(false)
}

async fn run_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
) -> anyhow::Result<SyncResult> {
    // Register before waiting on the account lock. Update/remove can request
    // cancellation while this rebuild is queued behind another operation.
    let cancel_receiver = state.mail_rebuild_cancellations.register(account.id);
    let _operation = state.account_operations.acquire(account.id).await;
    if let Err(error) = invalidate_searches_for_account(&state, account.id).await {
        state.mail_rebuild_cancellations.clear(account.id);
        release_mail_rebuild(&state, account.id);
        return Err(anyhow::Error::msg(error));
    }
    let current_account = match state.store.account(account.id).await {
        Err(error) => {
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(error);
        }
        Ok(Some(account)) => account,
        Ok(None) => {
            state
                .mail_rebuilds
                .lock()
                .expect("mail rebuild lock poisoned")
                .remove(&account.id);
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(anyhow::anyhow!("Account not found"));
        }
    };
    // The durable job is the source of truth while holding the account lock.
    // A queued worker may have captured an older `false` before an update
    // atomically replaced its job with `true`; never downgrade that reset.
    let durable_reset = match state.store.mail_rebuild_jobs().await {
        Ok(jobs) => jobs
            .into_iter()
            .find(|job| job.account_id == account.id)
            .map(|job| job.reset_before_sync),
        Err(error) => {
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(error);
        }
    };
    let reset_before_sync = effective_rebuild_reset(reset_before_sync, durable_reset);
    let result = run_mail_rebuild_locked(
        app,
        state.clone(),
        current_account,
        reset_before_sync,
        cancel_receiver,
    )
    .await;
    // This is deliberately outside the worker body: database failures before
    // the first checkpoint must not leave a queued cancellation or a stale
    // duplicate-run reservation behind.
    state.mail_rebuild_cancellations.clear(account.id);
    release_mail_rebuild(&state, account.id);
    result
}

async fn run_mail_rebuild_locked(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
    mut cancel_receiver: watch::Receiver<MailRebuildCancellationDisposition>,
) -> anyhow::Result<SyncResult> {
    let initial = MailRebuildJob {
        account_id: account.id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync,
    };
    let mut stopped_realtime = false;
    let result = async {
        if state
            .mail_rebuild_cancellations
            .disposition(&cancel_receiver)
            != MailRebuildCancellationDisposition::None
        {
            return Err(anyhow::anyhow!("mail rebuild cancelled"));
        }
        state.realtime.stop_account(account.id).await;
        stopped_realtime = true;
        state.store.save_mail_rebuild_job(&initial).await?;
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .insert(account.id, initial.clone().into());

        let service = MailService::new(state.store.clone());
        let progress_app = app.clone();
        let progress_state = state.clone();
        let account_id = account.id;
        let rebuild = async {
            if reset_before_sync {
                service
                    .rebuild_all_with_progress(&account, 250, move |progress| {
                        publish_mail_rebuild_progress(
                            &progress_app,
                            &progress_state,
                            account_id,
                            progress,
                        );
                    })
                    .await
            } else {
                service
                    .resume_rebuild_all_with_progress(&account, 250, move |progress| {
                        publish_mail_rebuild_progress(
                            &progress_app,
                            &progress_state,
                            account_id,
                            progress,
                        );
                    })
                    .await
            }
        };
        tokio::pin!(rebuild);
        tokio::select! {
            result = &mut rebuild => result,
            changed = cancel_receiver.changed() => {
                match changed {
                    Ok(()) if state.mail_rebuild_cancellations.disposition(&cancel_receiver)
                        != MailRebuildCancellationDisposition::None =>
                    {
                        Err(anyhow::anyhow!("mail rebuild cancelled"))
                    }
                    _ => rebuild.await,
                }
            }
        }
    }
    .await;
    let disposition = state
        .mail_rebuild_cancellations
        .disposition(&cancel_receiver);

    let mut result = result;
    let retains_durable_job = result.as_ref().err().is_some_and(|failure| {
        should_retain_mail_rebuild_job(failure, disposition, reset_before_sync)
    });
    if result.is_ok() {
        if let Err(error) = state.store.delete_mail_rebuild_job(account.id).await {
            result = Err(error);
        } else {
            // Sent-recipient learning is deliberately detached from the rebuild's
            // account-operation lock. The worker drains short restart-safe
            // batches, yielding between them, so a large Sent folder cannot make
            // the successful rebuild or foreground operations wait.
            let contacted_people_app = app.clone();
            let contacted_people_state = state.clone();
            tauri::async_runtime::spawn(async move {
                continue_contacted_people_backfill(contacted_people_app, contacted_people_state)
                    .await;
            });
        }
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
        if result.is_ok() {
            let _ = app.emit(
                "mail-index-rebuilt",
                serde_json::json!({ "accountId": account.id }),
            );
            kick_classification(state.clone());
        }
    } else if retains_durable_job {
        // Leave the persisted job intact so application startup can resume
        // this interrupted rebuild. Save the last published checkpoint before
        // clearing the in-memory entry, so a restart resumes with truthful
        // progress instead of the initial connecting state.
        let latest = state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
        if let Some(latest) = latest {
            if let Err(error) = state
                .store
                .save_mail_rebuild_job(&MailRebuildJob {
                    account_id: latest.account_id,
                    phase: latest.phase,
                    completed: latest.completed,
                    total: latest.total,
                    reset_before_sync: latest.reset_before_sync,
                })
                .await
            {
                tracing::warn!(
                    account_id = %account.id,
                    error = %error,
                    "could not persist failed mail rebuild checkpoint"
                );
            }
        }
    } else {
        if let Err(error) = state.store.delete_mail_rebuild_job(account.id).await {
            tracing::warn!(account_id = %account.id, error = %error, "could not delete abandoned mail rebuild job");
        }
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
    }
    let outcome = if result.is_ok() {
        "completed"
    } else if disposition != MailRebuildCancellationDisposition::None {
        "cancelled"
    } else {
        "failed"
    };
    let _ = app.emit(
        "mail-rebuild-finished",
        serde_json::json!({ "accountId": account.id, "outcome": outcome }),
    );
    // Do not restart realtime into an old catalogue while a replacement reset
    // remains reserved. The resumed rebuild owns restarting it on success.
    if stopped_realtime && !(retains_durable_job && reset_before_sync) {
        if let Err(error) = restart_realtime_if_current(app, &state, account.id).await {
            if result.is_ok() {
                return Err(error);
            }
            tracing::warn!(account_id = %account.id, error = %error, "could not restart realtime after mail rebuild");
        }
    }
    result
}

/// Drains historical Sent recipients in short transactions. Each batch drops
/// the account operation lock before yielding so normal message opens, sends,
/// and rebuilds can run between batches and restart-safe markers prevent
/// duplicate learning after an interruption.
async fn continue_contacted_people_backfill(app: tauri::AppHandle, state: Arc<AppState>) {
    drain_contacted_people_backfill(&state, || {
        emit_contacted_people_changed(&app, true, false);
    })
    .await;
}

/// Drain every eligible account in short Sent-recipient batches. A snapshot
/// can become stale while this worker yields, so each batch re-checks both the
/// global collection setting and the current account before acquiring data.
/// That makes disable/delete a clean stop rather than a background write.
async fn drain_contacted_people_backfill<F>(state: &Arc<AppState>, mut changed: F)
where
    F: FnMut(),
{
    if !state
        .store
        .autocomplete_suggestions_enabled()
        .await
        .unwrap_or(false)
    {
        return;
    }
    let accounts = match state.store.accounts().await {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::warn!(error = %error, "could not schedule contacted-people backfill");
            return;
        }
    };
    let excluded = accounts
        .iter()
        .map(|account| account.email.clone())
        .collect::<Vec<_>>();
    for account in accounts.into_iter().filter(|account| account.enabled) {
        if !state
            .store
            .autocomplete_suggestions_enabled()
            .await
            .unwrap_or(false)
        {
            return;
        }
        loop {
            // The setting can change while a previous account or batch was
            // running. Do not begin another transaction after disable.
            if !state
                .store
                .autocomplete_suggestions_enabled()
                .await
                .unwrap_or(false)
            {
                return;
            }
            let progress = {
                let _operation = state.account_operations.acquire(account.id).await;
                match state.store.account(account.id).await {
                    Ok(Some(current)) if current.enabled => state
                        .store
                        .backfill_contacted_people_from_sent(account.id, &excluded, 100)
                        .await
                        .map(Some),
                    Ok(_) => Ok(None),
                    Err(error) => Err(error),
                }
            };
            match progress {
                Ok(Some(progress)) => {
                    if contacted_people_backfill_changed(&progress) {
                        changed();
                    }
                    if progress.complete {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(account_id = %account.id, error = %error, "could not continue contacted-people backfill");
                    break;
                }
            }
        }
    }
}

fn contacted_people_backfill_changed(
    progress: &dakia_core::storage::ContactedPeopleBackfillProgress,
) -> bool {
    progress.changed_people > 0
}

/// Continues the additive search catalogue migration after Store::open has
/// performed its first bounded batch. No provider or account lock is held:
/// each call advances at most 500 rows per stage, then yields so foreground
/// opening and sending keep precedence.
async fn continue_search_catalogue_v2_backfill(state: Arc<AppState>) {
    loop {
        match state.store.advance_search_catalogue_v2_backfill().await {
            Ok(progress) if progress.complete => return,
            Ok(_) => tokio::task::yield_now().await,
            Err(error) => {
                tracing::warn!(error = %error, "could not continue search catalogue v2 backfill");
                return;
            }
        }
    }
}

/// Completes legacy contacted-people normalization and source-marker upgrades
/// after Store::open has performed its first bounded batch. These migrations
/// never learn an address or alter ranking statistics, so they intentionally
/// do not emit `contacted-people-changed`; open dropdowns need refreshes only
/// when recipient data itself changed.
async fn continue_contacted_people_migrations(state: Arc<AppState>) {
    loop {
        match state.store.continue_contacted_people_migrations(500).await {
            Ok(progress) if progress.complete => return,
            Ok(_) => tokio::task::yield_now().await,
            Err(error) => {
                tracing::warn!(error = %error, "could not continue contacted-people migrations");
                return;
            }
        }
    }
}

fn kick_contacted_people_migrations(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let drain = state.contacted_people_migration_drain.clone();
        let _drain = drain.lock().await;
        continue_contacted_people_migrations(state).await;
    });
}

#[tauri::command]
async fn mail_rebuild_status(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<MailRebuildProgress>, String> {
    Ok(state
        .mail_rebuilds
        .lock()
        .map_err(error)?
        .values()
        .cloned()
        .collect())
}

#[tauri::command]
async fn sync_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    limit: Option<u32>,
    full: Option<bool>,
    on_progress: Channel<SyncProgress>,
) -> Result<SyncResult, String> {
    let result = if full.unwrap_or(false) {
        if !reserve_mail_rebuild(state.inner(), account_id) {
            return Err("A mail re-index is already running for this account".to_owned());
        }
        let account = state.store.account(account_id).await.map_err(error);
        let account = match account
            .and_then(|account| account.ok_or_else(|| "Account not found".to_owned()))
        {
            Ok(account) => account,
            Err(error) => {
                release_mail_rebuild(state.inner(), account_id);
                return Err(error);
            }
        };
        let reset_before_sync = match state.mail_rebuilds.lock().map_err(error) {
            Ok(rebuilds) => rebuilds
                .get(&account_id)
                .map(|job| job.reset_before_sync)
                .unwrap_or(true),
            Err(lock_error) => {
                release_mail_rebuild(state.inner(), account_id);
                return Err(lock_error);
            }
        };
        if let Err(schedule_error) =
            schedule_mail_rebuild(state.inner(), account_id, reset_before_sync).await
        {
            release_mail_rebuild(state.inner(), account_id);
            return Err(error(schedule_error));
        }
        let result = run_mail_rebuild(
            app.clone(),
            state.inner().clone(),
            account.clone(),
            reset_before_sync,
        )
        .await;
        if result.is_ok() {
            let _ = on_progress.send(SyncProgress {
                phase: "complete",
                completed: 1,
                total: Some(1),
            });
        }
        (result, account)
    } else {
        let _operation = state.account_operations.acquire(account_id).await;
        // A foreground refresh may publish provider-authoritative flags and
        // locators. Invalidate concurrent hybrid search publications before
        // the refresh begins; the generation guard then rejects late IMAP
        // search writes atomically in Storage.
        invalidate_searches_for_account(state.inner(), account_id).await?;
        let account = state
            .store
            .account(account_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Account not found".to_owned())?;
        state.realtime.stop_account(account_id).await;
        let service = MailService::new(state.store.clone());
        let result = service
            .refresh_inbox_with_progress(&account, limit.unwrap_or(50), |progress| {
                let _ = on_progress.send(progress);
            })
            .await;
        let result = complete_manual_sync_attempt(
            result,
            restart_realtime_if_current(app.clone(), state.inner(), account_id).await,
            account_id,
        );
        (result, account)
    };
    let (result, _) = result;
    let synced = result.map_err(error)?;
    kick_classification(state.inner().clone());
    Ok(synced)
}

#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    draft: ComposeMessage,
) -> Result<String, String> {
    let _operation = state.account_operations.acquire(draft.account_id).await;
    let account = enabled_account_for_operation(state.inner(), draft.account_id).await?;
    let response = MailService::new(state.store.clone())
        .send(&account, &draft)
        .await
        .map_err(error)?;
    // `MailService::send` records recipients only after the final SMTP DATA
    // acceptance. Do not publish this event on a failed transaction.
    if state
        .store
        .autocomplete_suggestions_enabled()
        .await
        .unwrap_or(false)
    {
        emit_contacted_people_changed(&app, true, false);
    }
    Ok(response)
}

#[tauri::command]
async fn apply_mailbox_action(
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    mailbox: String,
    uid: u32,
    action: MailboxAction,
) -> Result<(), String> {
    let _operation = state.account_operations.acquire(account_id).await;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    require_permanent_delete_locator(&state.store, account_id, &mailbox, uid, action).await?;
    let destination_uid = MailService::new(state.store.clone())
        .apply_action(&account, &mailbox, uid, action)
        .await
        .map_err(error)?;
    state
        .store
        .move_message(
            account.id,
            &mailbox,
            uid,
            mailbox_action_destination(action).unwrap_or_default(),
            destination_uid,
        )
        .await
        .map_err(error)
}

async fn require_permanent_delete_locator(
    store: &Store,
    account_id: Uuid,
    mailbox: &str,
    uid: u32,
    action: MailboxAction,
) -> Result<(), String> {
    if !matches!(action, MailboxAction::Delete) {
        return Ok(());
    }
    store
        .message_by_locator(account_id, mailbox, uid)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message is no longer available in this mailbox".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod permanent_delete_command_tests {
    use super::*;
    use chrono::Utc;

    fn message(account_id: Uuid, mailbox: &str, uid: u32) -> MailSummary {
        MailSummary {
            id: format!("{account_id}:{mailbox}:{uid}"),
            account_id: account_id.to_string(),
            mailbox: mailbox.into(),
            uid: i64::from(uid),
            message_id: Some(format!("<{uid}@example.test>")),
            in_reply_to: None,
            reference_ids: None,
            thread_id: format!("thread-{uid}"),
            subject: "Permanent delete locator".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "reader@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: String::new(),
            body_text: String::new(),
            body_html: None,
            content_state: "headers_only".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: false,
            is_flagged: false,
            is_answered: false,
            is_draft: false,
            has_attachments: false,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: vec![],
        }
    }

    #[tokio::test]
    async fn permanent_delete_requires_the_exact_local_account_mailbox_and_uid() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account_id = Uuid::new_v4();
        let other_account_id = Uuid::new_v4();
        store
            .upsert_messages(&[message(account_id, "INBOX", 42)])
            .await
            .expect("save message");

        require_permanent_delete_locator(&store, account_id, "INBOX", 42, MailboxAction::Delete)
            .await
            .expect("exact locator is accepted");
        for (candidate_account, candidate_mailbox, candidate_uid) in [
            (other_account_id, "INBOX", 42),
            (account_id, "Archive", 42),
            (account_id, "INBOX", 41),
        ] {
            assert!(
                require_permanent_delete_locator(
                    &store,
                    candidate_account,
                    candidate_mailbox,
                    candidate_uid,
                    MailboxAction::Delete,
                )
                .await
                .is_err(),
                "a crossed or absent locator must fail before IMAP"
            );
        }
        require_permanent_delete_locator(
            &store,
            other_account_id,
            "INBOX",
            42,
            MailboxAction::Trash,
        )
        .await
        .expect("ordinary Trash behavior remains unchanged");
    }
}

#[tauri::command]
async fn unsubscribe_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<UnsubscribeResult, String> {
    // Unsubscribe metadata is parsed, verified, and persisted when the message
    // is synced. Do not make an unrelated IMAP round trip before acting on it:
    // that made otherwise valid unsubscribe actions fail whenever the mailbox
    // could not be fetched again.
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    let mut cleanup_target = sender_cleanup_target(&message, account_id);
    let _operation = state.account_operations.acquire(account_id).await;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    let service = MailService::new(state.store.clone());
    let outcome = match service.unsubscribe(&message).await {
        Ok(outcome) => outcome,
        // Older catalogue rows may contain an action selected by a previous
        // parser version. Refresh only malformed, side-effect-free web/mailto
        // metadata so a later valid fallback in the header can be selected.
        // Never retry a one-click POST: its failure may be ambiguous.
        Err(_) if message.unsubscribe_kind.as_deref() != Some("one_click") => {
            let refreshed = fetch_remote_message(state.inner(), &message_id).await?;
            cleanup_target = sender_cleanup_target(&refreshed, account_id);
            service.unsubscribe(&refreshed).await.map_err(error)?
        }
        Err(failure) => return Err(error(failure)),
    };
    match outcome {
        UnsubscribeOutcome::Completed => Ok(UnsubscribeResult::Completed { cleanup_target }),
        UnsubscribeOutcome::Web(url) => {
            open_external_url(app, url)?;
            Ok(UnsubscribeResult::OpenedWeb { cleanup_target })
        }
        UnsubscribeOutcome::Mailto { to, subject, body } => {
            let draft = unsubscribe_email(account_id, to, subject, body)?;
            MailService::new(state.store.clone())
                .send(&account, &draft)
                .await
                .map_err(error)?;
            Ok(UnsubscribeResult::Completed { cleanup_target })
        }
    }
}

#[tauri::command]
async fn trash_messages_from_sender(
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    sender_address: String,
) -> Result<SenderTrashResult, String> {
    let _operation = state.account_operations.acquire(account_id).await;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .trash_messages_from_sender(&account, &sender_address)
        .await
        .map_err(error)
}

#[tauri::command]
async fn ai_summarize(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<String, String> {
    let messages = hydrate_messages(state.inner(), &input.message_ids).await?;
    ai_service(&state.store, input)
        .await?
        .summarize(&messages)
        .await
        .map_err(error)
}

#[tauri::command]
async fn ai_draft(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<String, String> {
    let messages = hydrate_messages(state.inner(), &input.message_ids).await?;
    let instruction = input.instruction.clone().unwrap_or_default();
    ai_service(&state.store, input)
        .await?
        .draft(&instruction, &messages)
        .await
        .map_err(error)
}

async fn hydrate_messages(
    state: &Arc<AppState>,
    message_ids: &[String],
) -> Result<Vec<MailSummary>, String> {
    let hydration_state = state.clone();
    run_bounded_ordered(
        message_ids.to_vec(),
        MESSAGE_HYDRATION_CONCURRENCY,
        state.remote_operation_slots.clone(),
        move |message_id| {
            let state = hydration_state.clone();
            async move { hydrated_message(&state, &message_id).await }
        },
    )
    .await
    .into_iter()
    .collect()
}

#[tauri::command]
async fn ai_available(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<bool, String> {
    Ok(ai_service(&state.store, input).await?.is_available().await)
}

#[tauri::command]
async fn send_desktop_notification(
    app: tauri::AppHandle,
    notification: DesktopNotification,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut builder = notify_rust::Notification::new();
        builder
            .summary(&notification.title)
            .body(&notification.body)
            .auto_icon();
        if let Some(sound) = &notification.sound {
            builder.sound_name(sound);
        }
        let handle = builder.show().map_err(error)?;
        let action_app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            handle.wait_for_action(move |action| {
                if action == "__closed" {
                    return;
                }
                if !notification_has_reader_target(&notification) {
                    if let Some(window) = action_app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                let _ = action_app.emit("notification-action", notification);
            });
        });
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        let _ = notification;
        Err("Native desktop notifications are only used on macOS".to_owned())
    }
}

async fn ai_service(store: &Store, input: AiInput) -> Result<AiService, String> {
    let provider = match input.provider.as_str() {
        "ollama" => AiProvider::Ollama {
            base_url: Url::parse(
                input
                    .base_url
                    .as_deref()
                    .unwrap_or("http://127.0.0.1:11434/"),
            )
            .map_err(error)?,
            model: input.model,
        },
        "openai" => AiProvider::OpenAiCompatible {
            base_url: Url::parse(
                input
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1/"),
            )
            .map_err(error)?,
            model: input.model,
        },
        "local" => AiProvider::LocalCommand {
            executable: input
                .executable
                .ok_or_else(|| "Local AI executable is required".to_owned())?,
            model_path: input
                .model_path
                .ok_or_else(|| "Local model path is required".to_owned())?,
            extra_args: Vec::new(),
        },
        _ => return Err("Unknown AI provider".into()),
    };
    let api_key = match input.api_key.filter(|value| !value.is_empty()) {
        Some(value) => Some(value),
        None => store
            .secret("dev.dakia.mail:ai:api-key")
            .await
            .map_err(error)?,
    }
    .map(SecretString::from);
    Ok(AiService::new(AiConfig { provider, api_key }))
}

#[tauri::command]
async fn set_ai_api_key(state: State<'_, Arc<AppState>>, api_key: String) -> Result<(), String> {
    if api_key.is_empty() {
        state
            .store
            .delete_secret("dev.dakia.mail:ai:api-key")
            .await
            .map_err(error)?;
    } else {
        state
            .store
            .set_secret("dev.dakia.mail:ai:api-key", &api_key)
            .await
            .map_err(error)?;
    }
    Ok(())
}

#[tauri::command]
fn translation_models(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<TranslationModelStatus>, String> {
    translation::statuses(&state.data_dir)
}

#[tauri::command]
fn translation_model_files(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<TranslationModelFiles, String> {
    translation::files(&state.data_dir, &source)
}

#[tauri::command]
fn translation_detect_language(text: String) -> TranslationLanguageDetection {
    translation::detect_language(&text)
}

#[tauri::command]
async fn translation_install_model(
    state: State<'_, Arc<AppState>>,
    source: String,
    on_progress: Channel<TranslationDownloadProgress>,
) -> Result<TranslationModelFiles, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    state
        .translation_downloads
        .lock()
        .map_err(error)?
        .insert(source.clone(), cancelled.clone());
    let result = translation::install(&state.data_dir, &source, on_progress, cancelled).await;
    state
        .translation_downloads
        .lock()
        .map_err(error)?
        .remove(&source);
    result
}

#[tauri::command]
fn translation_cancel_install(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<(), String> {
    if let Some(cancelled) = state
        .translation_downloads
        .lock()
        .map_err(error)?
        .get(&source)
    {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
async fn translation_remove_model(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<(), String> {
    translation::remove(&state.data_dir, &source).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let dropped_file_receipts = Arc::new(DroppedFileReceiptStore::default());
    let event_receipts = dropped_file_receipts.clone();
    let window_receipts = dropped_file_receipts.clone();
    tauri::Builder::default()
        .manage(dropped_file_receipts)
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_menu_event(|app, event| {
            let action = event.id().as_ref();
            if let Some(encoded_address) = action.strip_prefix("copy-email-address:") {
                match decode_context_menu_email_address(encoded_address)
                    .and_then(|address| app.clipboard().write_text(address).map_err(error))
                {
                    Ok(()) => {}
                    Err(copy_error) => {
                        tracing::warn!(error = %copy_error, "could not copy email address");
                        let target = app
                            .webview_windows()
                            .into_values()
                            .find(|window| window.is_focused().unwrap_or(false))
                            .or_else(|| app.get_webview_window("main"));
                        if let Some(window) = target {
                            let _ = window.emit("menu-action", "copy-email-address-failed");
                        }
                    }
                }
                return;
            }
            let focused = menu_action_targets_focused_window(action).then(|| {
                app.webview_windows()
                    .into_values()
                    .find(|window| window.is_focused().unwrap_or(false))
            });
            if let Some(Some(window)) = focused {
                let _ = window.emit("menu-action", action);
            } else if let Some(main) = app.get_webview_window("main") {
                let _ = main.emit("menu-action", action);
            }
        })
        .on_window_event(move |window, event| match event {
            WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                match event_receipts.issue(window.label(), paths.clone()) {
                    Ok(receipt) => {
                        let expiry_receipts = event_receipts.clone();
                        let expiry_receipt = receipt.clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(DROPPED_FILE_RECEIPT_TTL).await;
                            expiry_receipts.expire_at(&expiry_receipt, Instant::now());
                        });
                        if let Err(emit_error) = window.emit(DROPPED_FILE_RECEIPT_EVENT, receipt) {
                            tracing::warn!(
                                error = %emit_error,
                                window = window.label(),
                                "could not deliver dropped-file receipt"
                            );
                        }
                    }
                    Err(drop_error) => {
                        if let Err(emit_error) = window.emit(DROPPED_FILE_ERROR_EVENT, drop_error) {
                            tracing::warn!(
                                error = %emit_error,
                                window = window.label(),
                                "could not deliver dropped-file error"
                            );
                        }
                    }
                }
            }
            WindowEvent::Destroyed => {
                window_receipts.revoke_window(window.label());
            }
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                // notify-rust uses process-global state for the application identity. It
                // rejects subsequent calls, so initialize it once before either the
                // frontend notification plugin or realtime mail notifications run.
                let application = if tauri::is_dev() {
                    "com.apple.Terminal"
                } else {
                    app.config().identifier.as_str()
                };
                notify_rust::set_application(application)
                    .map_err(|error| anyhow::anyhow!("notification application: {error}"))?;
            }
            install_app_menu(app).map_err(|error| anyhow::anyhow!("app menu: {error}"))?;
            let release_smoke_test = std::env::var_os("DAKIA_RELEASE_SMOKE_TEST").as_deref()
                == Some(std::ffi::OsStr::new("1"));
            let data_dir = if release_smoke_test {
                std::env::var_os("DAKIA_RELEASE_SMOKE_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "DAKIA_RELEASE_SMOKE_DATA_DIR is required for release smoke tests"
                        )
                    })?
            } else {
                app.path()
                    .app_local_data_dir()
                    .map_err(|error| anyhow::anyhow!("local data directory: {error}"))?
            };
            let resource_dir = match app.path().resource_dir() {
                Ok(path) => path,
                Err(_) => std::env::current_exe()?
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("development executable has no parent"))?
                    .to_path_buf(),
            };
            let classifier_dir = resource_dir.join("resources/email-classifier-v2");
            let state = tauri::async_runtime::block_on(async {
                let store = Store::open(data_dir.join("dakia.db")).await?;
                let classifier = LocalEmailClassifier::from_dir(&classifier_dir)?;
                let mail_rebuilds = store
                    .mail_rebuild_jobs()
                    .await?
                    .into_iter()
                    .map(|job| (job.account_id, job.into()))
                    .collect();
                anyhow::Ok(Arc::new(AppState {
                    realtime: RealtimeSyncManager::new(store.clone()),
                    store,
                    data_dir,
                    classifier: Mutex::new(Box::new(classifier)),
                    classification_owner: Uuid::new_v4().to_string(),
                    classification: Arc::new(ClassificationScheduler::default()),
                    mail_rebuilds: Mutex::new(mail_rebuilds),
                    mail_rebuild_running: Mutex::new(HashSet::new()),
                    mail_rebuild_cancellations: MailRebuildCancellations::default(),
                    account_operations: AccountOperationLocks::default(),
                    search_sessions: SearchSessionRegistry::default(),
                    remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
                    remote_search_slots: Arc::new(Semaphore::new(REMOTE_SEARCH_CONCURRENCY)),
                    translation_downloads: Mutex::new(HashMap::new()),
                    contacted_people_migration_drain: Arc::new(AsyncMutex::new(())),
                }))
            })?;
            app.manage(state.clone());
            let classification_state = Arc::downgrade(&state);
            state
                .realtime
                .set_hydration_complete_hook(Arc::new(move || {
                    if let Some(state) = classification_state.upgrade() {
                        kick_classification(state);
                    }
                }));
            if release_smoke_test {
                eprintln!("DAKIA_RELEASE_SMOKE_TEST_OK");
                app.handle().exit(0);
                return Ok(());
            }
            // Real-time mail is a native application responsibility. Starting
            // it here keeps delivery alive even when the webview is slow to
            // mount, hidden by --background, or temporarily unavailable.
            let realtime_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                kick_classification(state.clone());
                let contacted_people_state = state.clone();
                let contacted_people_app = realtime_app.clone();
                tauri::async_runtime::spawn(async move {
                    continue_contacted_people_backfill(
                        contacted_people_app,
                        contacted_people_state,
                    )
                    .await;
                });
                kick_contacted_people_migrations(state.clone());
                let search_catalogue_state = state.clone();
                tauri::async_runtime::spawn(async move {
                    continue_search_catalogue_v2_backfill(search_catalogue_state).await;
                });
                let rebuilding: HashMap<_, _> = state
                    .mail_rebuilds
                    .lock()
                    .expect("mail rebuild lock poisoned")
                    .iter()
                    .map(|(account_id, job)| (*account_id, job.reset_before_sync))
                    .collect();
                match state.store.accounts().await {
                    Ok(accounts) => {
                        for account in accounts {
                            if let Some(reset_before_sync) = rebuilding.get(&account.id) {
                                let rebuild_app = realtime_app.clone();
                                let rebuild_state = state.clone();
                                let reset_before_sync = *reset_before_sync;
                                if !reserve_mail_rebuild(&rebuild_state, account.id) {
                                    continue;
                                }
                                tauri::async_runtime::spawn(async move {
                                    if let Err(error) = run_mail_rebuild(
                                        rebuild_app,
                                        rebuild_state,
                                        account,
                                        reset_before_sync,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            error = %error,
                                            "could not resume interrupted mail rebuild"
                                        );
                                    }
                                });
                            } else if account.enabled {
                                state
                                    .realtime
                                    .start_account(realtime_app.clone(), account)
                                    .await;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "could not start native mail tasks");
                    }
                }
            });
            if std::env::args().any(|argument| argument == "--background") {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            provider_presets,
            message_content,
            configure_tray,
            terminal_command_status,
            install_terminal_command,
            remove_terminal_command,
            message_attachments,
            save_attachment,
            export_message,
            save_all_attachments,
            forward_attachments,
            read_dropped_files,
            accounts,
            list_search_mailboxes,
            update_account,
            show_account_context_menu,
            show_email_address_context_menu,
            remove_account,
            open_external_url,
            add_account,
            search,
            search_smart_inbox,
            suggest_contacted_people,
            hide_contacted_person,
            clear_contacted_people,
            get_autocomplete_settings,
            set_autocomplete_settings,
            validate_compose_recipients,
            conversation_for_target,
            search_remote,
            start_search,
            next_search_page,
            cancel_search,
            set_message_category,
            set_message_starred,
            set_message_read,
            starred_conversation_count,
            classify_pending,
            start_realtime_sync,
            reconcile_realtime_sync,
            realtime_sync_status,
            record_notification_delivered,
            hydrate_message,
            mail_rebuild_status,
            sync_account,
            send_message,
            apply_mailbox_action,
            unsubscribe_message,
            trash_messages_from_sender,
            ai_summarize,
            ai_draft,
            ai_available,
            send_desktop_notification,
            set_ai_api_key,
            translation_models,
            translation_model_files,
            translation_detect_language,
            translation_install_model,
            translation_cancel_install,
            translation_remove_model
        ])
        .run(tauri::generate_context!())
        .expect("Dakia desktop runtime failed");
}
