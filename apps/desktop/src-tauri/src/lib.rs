mod operations;
mod outgoing;
mod realtime;
mod translation;

#[cfg(test)]
mod tauri_contracts_tests;

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use dakia_core::storage::{
    ConversationTarget, MessageContentFetchAcquire, MessageRemoteIdentity, SyncRun, SyncRunUpdate,
};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    normalize_sender_address, provider, Account, AccountAuth, AccountDraft, Attachment,
    CachedMessageContent, ComposeMessage, EmailClassificationInput, LocalEmailClassifier,
    MailConversation, MailConversationPage, MailRebuildJob, MailService, MailSummary,
    MailboxAction, ModelClassificationUpdate, PreparedOutgoingMessage, ProviderPreset, SearchQuery,
    SendOutcome, SenderTrashResult, SentCopyOutcome, SentCopyPresence, SentCopyStatus,
    SmartInboxPage, SmartInboxQuery, Store, SupportedMailbox, SyncProgress, SyncResult,
    UnsubscribeOutcome,
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
    DragDropEvent, Emitter, Listener, Manager, State, WindowEvent,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{watch, Mutex as AsyncMutex, Notify, OwnedMutexGuard, Semaphore};
use url::Url;
use uuid::Uuid;

use outgoing::{ClaimHeartbeat, SubmissionCoordinator};
use realtime::{RealtimeSyncManager, RealtimeSyncStatus};
use translation::{
    TranslationDownloadProgress, TranslationLanguageDetection, TranslationModelFiles,
    TranslationModelStatus,
};

const REMOTE_SEARCH_CONCURRENCY: usize = 4;
const MESSAGE_HYDRATION_CONCURRENCY: usize = 4;
const CLASSIFICATION_BATCH_SIZE: usize = 64;
const CLASSIFICATION_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(500)];
const MAX_EXPORT_FILENAME_BYTES: usize = 255;
const MAX_DOWNLOAD_COLLISION_SUFFIX_BYTES: usize = " (9999)".len();
const ACCOUNT_REMOVAL_SUBMISSION_WAIT: Duration = Duration::from_secs(10);
const OUTGOING_SUBMISSION_CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SENT_COPY_CATALOGUE_REFRESH_LIMIT: u32 = 24;
const SENT_COPY_CATALOGUE_REFRESH_LOOKBACK_DAYS: i64 = 14;

struct AppState {
    store: Store,
    data_dir: PathBuf,
    classifier: Mutex<Box<dyn EmailClassifier>>,
    classification_owner: String,
    classification: Arc<ClassificationScheduler>,
    realtime: RealtimeSyncManager,
    remote_operation_slots: Arc<Semaphore>,
    mail_rebuilds: Mutex<HashMap<Uuid, MailRebuildProgress>>,
    mail_rebuild_running: Mutex<HashSet<Uuid>>,
    mail_rebuild_cancellations: MailRebuildCancellations,
    progressive_sync_cancellations: ProgressiveSyncCancellations,
    sync_runs_running: Mutex<HashSet<Uuid>>,
    mutation_drains_running: Mutex<HashSet<Uuid>>,
    folder_promotions_running: Mutex<HashSet<(Uuid, String)>>,
    sent_reconcile_retry_running: Mutex<HashSet<Uuid>>,
    sent_reconcile_poll_running: Mutex<HashSet<Uuid>>,
    account_operations: AccountOperationLocks,
    submissions: Arc<SubmissionCoordinator>,
    translation_downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
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

    fn clear_request(&self, account_id: Uuid) {
        if let Some(sender) = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned")
            .get(&account_id)
            .map(|cancellation| cancellation.sender.clone())
        {
            sender.send_replace(MailRebuildCancellationDisposition::None);
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

/// Cancellation for first-Inbox/history/promotion work. These workers make
/// bounded provider calls, so lifecycle changes can wait for the next command
/// boundary rather than deleting account state underneath live IMAP work.
#[derive(Default)]
struct ProgressiveSyncCancellations {
    active: Mutex<HashMap<Uuid, ProgressiveSyncCancellation>>,
}

struct ProgressiveSyncCancellation {
    sender: watch::Sender<bool>,
    registrations: usize,
    blocked: bool,
}

impl ProgressiveSyncCancellations {
    fn register(&self, account_id: Uuid) -> watch::Receiver<bool> {
        let mut active = self
            .active
            .lock()
            .expect("progressive sync cancellation lock poisoned");
        let entry = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(false);
            ProgressiveSyncCancellation {
                sender,
                registrations: 0,
                blocked: false,
            }
        });
        entry.registrations += 1;
        entry.sender.subscribe()
    }

    fn cancel(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("progressive sync cancellation lock poisoned");
        let entry = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(false);
            ProgressiveSyncCancellation {
                sender,
                registrations: 0,
                blocked: false,
            }
        });
        entry.blocked = true;
        entry.sender.send_replace(true);
    }

    fn resume(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("progressive sync cancellation lock poisoned");
        if let Some(entry) = active.get_mut(&account_id) {
            entry.blocked = false;
            entry.sender.send_replace(false);
            if entry.registrations == 0 {
                active.remove(&account_id);
            }
        }
    }

    fn clear(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("progressive sync cancellation lock poisoned");
        if let Some(entry) = active.get_mut(&account_id) {
            entry.registrations -= 1;
            if entry.registrations == 0 && !entry.blocked {
                active.remove(&account_id);
            }
        }
    }
}

fn progressive_sync_cancelled(receiver: &watch::Receiver<bool>) -> bool {
    *receiver.borrow()
}

async fn wait_for_progressive_sync_quiescence(
    state: &Arc<AppState>,
    account_id: Uuid,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let initial_running = state
            .sync_runs_running
            .lock()
            .expect("sync run reservation lock poisoned")
            .contains(&account_id);
        let promotion_running = state
            .folder_promotions_running
            .lock()
            .expect("folder promotion reservation lock poisoned")
            .iter()
            .any(|(id, _)| *id == account_id);
        if !initial_running && !promotion_running {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn normalized_account_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn acceptance_data_dir() -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = std::env::var_os("DAKIA_ACCEPTANCE_DATA_DIR") else {
        return Ok(None);
    };
    if !cfg!(debug_assertions) {
        anyhow::bail!("DAKIA_ACCEPTANCE_DATA_DIR is available only in debug builds");
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        anyhow::bail!("DAKIA_ACCEPTANCE_DATA_DIR must be an absolute path");
    }
    std::fs::create_dir_all(&path)?;
    let path = path.canonicalize()?;
    if !is_dedicated_acceptance_dir(&path) {
        anyhow::bail!("DAKIA_ACCEPTANCE_DATA_DIR must name a dedicated directory");
    }
    if !path.is_dir() {
        anyhow::bail!("DAKIA_ACCEPTANCE_DATA_DIR must name a directory");
    }
    Ok(Some(path))
}

fn is_dedicated_acceptance_dir(path: &Path) -> bool {
    path != Path::new("/") && path != Path::new("/private") && path != Path::new("/private/tmp")
}

#[cfg(test)]
mod acceptance_data_dir_tests {
    use super::*;

    #[test]
    fn rejects_non_dedicated_acceptance_roots() {
        for root in [
            Path::new("/"),
            Path::new("/private"),
            Path::new("/private/tmp"),
        ] {
            assert!(!is_dedicated_acceptance_dir(root));
        }
        assert!(is_dedicated_acceptance_dir(Path::new(
            "/private/tmp/dakia-acceptance"
        )));
    }
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

    // A foreground open has its own claim class. It can immediately take over
    // a background warmer, while remaining serialized with another foreground
    // fetch of the same message.
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
        let identity = state
            .store
            .capture_message_remote_identity(message_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Message changed while it was being opened".to_owned())?;
        let message = fetch_remote_message(state, &identity).await?;
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
            persist_foreground_message_content(&state.store, &identity, &message, &cached).await?;
        if !still_starred {
            let cached_current = state
                .store
                .cache_message_content_if_current(&identity, false, cached.clone())
                .await
                .map_err(error)?;
            if !cached_current {
                return Err("Message changed while its content was loading".to_owned());
            }
        }
        let completed = state
            .store
            .set_message_content_state_if_current(&identity, "complete")
            .await
            .map_err(error)?;
        if !completed {
            return Err("Message changed while its content was loading".to_owned());
        }
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
            progressive_sync_cancellations: ProgressiveSyncCancellations::default(),
            sync_runs_running: Mutex::new(HashSet::new()),
            mutation_drains_running: Mutex::new(HashSet::new()),
            folder_promotions_running: Mutex::new(HashSet::new()),
            sent_reconcile_retry_running: Mutex::new(HashSet::new()),
            sent_reconcile_poll_running: Mutex::new(HashSet::new()),
            account_operations: AccountOperationLocks::default(),
            submissions: Arc::new(SubmissionCoordinator::default()),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            translation_downloads: Mutex::new(HashMap::new()),
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
    let (identity, account) =
        capture_remote_identity_and_account(state.inner(), &message_id).await?;
    let attachment = MailService::new(state.store.clone())
        .fetch_attachment_for_identity(&account, &identity, &attachment_id)
        .await
        .map_err(error)?;
    if !state
        .store
        .message_remote_identity_is_current(&identity)
        .await
        .map_err(error)?
    {
        return Err("Message changed while its attachment was loading".to_owned());
    }
    save_to_downloads(&app, &attachment.attachment, &attachment.bytes).map_err(error)
}

#[tauri::command]
async fn export_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<String, String> {
    let (initial_identity, _initial_account) =
        capture_remote_identity_and_account(state.inner(), &message_id).await?;
    let account_id = Uuid::parse_str(&initial_identity.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let (identity, account) =
        capture_remote_identity_and_account(state.inner(), &message_id).await?;
    if identity != initial_identity {
        return Err("Message changed while it was being exported".to_owned());
    }
    let bytes = MailService::new(state.store.clone())
        .fetch_raw_message_for_identity(&account, &identity)
        .await
        .map_err(error)?;
    if !state
        .store
        .message_remote_identity_is_current(&identity)
        .await
        .map_err(error)?
    {
        return Err("Message changed while it was being exported".to_owned());
    }
    let subject = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .map(|message| message.subject)
        .ok_or_else(|| "Message changed while it was being exported".to_owned())?;
    save_eml_to_downloads(&app, &subject, &bytes).map_err(error)
}

#[tauri::command]
async fn save_all_attachments(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
) -> Result<Vec<String>, String> {
    let (_, fetched) = fetch_full_remote_message(state.inner(), &message_id).await?;
    let attachments = fetched
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
    let (_, fetched) = fetch_full_remote_message(state.inner(), &message_id).await?;
    let attachments = fetched
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
    identity: &MessageRemoteIdentity,
    message: &MailSummary,
    content: &CachedMessageContent,
) -> Result<bool, String> {
    let exists = store
        .update_message_attachment_state_if_current(identity, message.has_attachments)
        .await
        .map_err(error)?;
    if !exists {
        return Ok(false);
    }
    if !message.is_flagged {
        return Ok(false);
    }
    store
        .cache_starred_message_content_if_current(identity, content.clone())
        .await
        .map_err(error)
}

#[cfg(test)]
mod attachment_presentation_command_tests {
    use super::*;
    use chrono::Utc;
    use dakia_core::{storage::AttachmentData, AttachmentPresentation};
    use tempfile::tempdir;

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
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 1, 1, true)
            .await
            .expect("save Inbox catalogue identity");
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
        let identity = store
            .capture_message_remote_identity(&message_id)
            .await
            .expect("capture identity")
            .expect("message identity");
        assert!(persist_foreground_message_content(
            &store,
            &identity,
            &fetched,
            &cached_content(&fetched)
        )
        .await
        .expect("persist authoritative foreground fetch"));

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
        let identity = store
            .capture_message_remote_identity(&message_id)
            .await
            .expect("capture identity")
            .expect("message identity");
        assert!(!persist_foreground_message_content(
            &store,
            &identity,
            &fetched,
            &cached_content(&fetched)
        )
        .await
        .expect("persist ordinary foreground metadata"));
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
        let identity = store
            .capture_message_remote_identity(&message_id)
            .await
            .expect("capture identity")
            .expect("message identity");
        assert!(!persist_foreground_message_content(
            &store,
            &identity,
            &fetched,
            &cached_content(&fetched)
        )
        .await
        .expect("persist stale fetch without resurrecting the star"));

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
}

async fn fetch_remote_message(
    state: &Arc<AppState>,
    identity: &MessageRemoteIdentity,
) -> Result<MailSummary, String> {
    let account_id = Uuid::parse_str(&identity.account_id).map_err(error)?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    MailService::new(state.store.clone())
        .fetch_message_for_identity(&account, identity, usize::MAX)
        .await
        .map_err(error)
}

async fn fetch_full_remote_message(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<(MessageRemoteIdentity, MailSummary), String> {
    let (identity, account) = capture_remote_identity_and_account(state, message_id).await?;
    let message = MailService::new(state.store.clone())
        .fetch_full_message_for_identity(&account, &identity)
        .await
        .map_err(error)?;
    if !state
        .store
        .message_remote_identity_is_current(&identity)
        .await
        .map_err(error)?
    {
        return Err("Message changed while it was loading".to_owned());
    }
    Ok((identity, message))
}

async fn capture_remote_identity_and_account(
    state: &Arc<AppState>,
    message_id: &str,
) -> Result<(MessageRemoteIdentity, Account), String> {
    let identity = state
        .store
        .capture_message_remote_identity(message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&identity.account_id).map_err(error)?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    Ok((identity, account))
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
        let before = MessageRemoteIdentity {
            message_id: "message".into(),
            account_id: Uuid::nil().to_string(),
            account_config_generation: 1,
            account_config_fingerprint: "fixture".into(),
            mailbox: "INBOX".into(),
            remote_name: "INBOX".into(),
            uid: 42,
            uid_validity: 10,
        };
        let mut reused_uid = before.clone();
        reused_uid.uid_validity = 11;

        assert_eq!(before, before);
        assert_ne!(before, reused_uid);
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

async fn release_account_operation_gate(gate: dakia_core::storage::AccountOperationGate) {
    if let Err(release_error) = gate.release().await {
        tracing::warn!(error = %release_error, "could not release account operation gate");
    }
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
    let durable_gate = state
        .store
        .acquire_account_removal_gate(input.id)
        .await
        .map_err(error)?;
    let durable_gate = durable_gate.ok_or_else(|| {
        "Account settings are already being changed. Try saving again after the current operation finishes."
            .to_owned()
    })?;
    let cross_process_quiet = match wait_for_cross_process_account_operations(
        &state.store,
        input.id,
        ACCOUNT_REMOVAL_SUBMISSION_WAIT,
    )
    .await
    {
        Ok(quiet) => quiet,
        Err(wait_error) => {
            release_account_operation_gate(durable_gate).await;
            return Err(error(wait_error));
        }
    };
    if !cross_process_quiet {
        release_account_operation_gate(durable_gate).await;
        return Err("Mail is still finishing an account operation. Try saving account settings again after it finishes.".to_owned());
    }
    let submission_pause = match state
        .submissions
        .pause(input.id, ACCOUNT_REMOVAL_SUBMISSION_WAIT)
        .await
    {
        Ok(pause) => pause,
        Err(_) => {
            release_account_operation_gate(durable_gate).await;
            return Err(
                "An email is still being submitted. Try saving account settings again after it finishes."
                    .to_owned(),
            );
        }
    };
    // Ask an existing rebuild to stop before waiting for its account lock. A
    // normal settings or credential update retains the durable job so it can
    // resume with the new connection details.
    request_mail_rebuild_cancel(
        state.inner(),
        input.id,
        MailRebuildCancellationDisposition::Retain,
    );
    state.progressive_sync_cancellations.cancel(input.id);
    if !wait_for_progressive_sync_quiescence(
        state.inner(),
        input.id,
        ACCOUNT_REMOVAL_SUBMISSION_WAIT,
    )
    .await
    {
        state.progressive_sync_cancellations.resume(input.id);
        state.mail_rebuild_cancellations.clear_request(input.id);
        release_account_operation_gate(durable_gate).await;
        drop(submission_pause);
        return Err(
            "Mail sync is still finishing a provider request. Try saving account settings again after it finishes."
                .to_owned(),
        );
    }
    let _operation = state.account_operations.acquire(input.id).await;
    let mut account = match state.store.account(input.id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            state.mail_rebuild_cancellations.clear_request(input.id);
            state.progressive_sync_cancellations.resume(input.id);
            release_account_operation_gate(durable_gate).await;
            drop(submission_pause);
            return Err("Account not found".to_owned());
        }
        Err(failure) => {
            state.mail_rebuild_cancellations.clear_request(input.id);
            state.progressive_sync_cancellations.resume(input.id);
            release_account_operation_gate(durable_gate).await;
            drop(submission_pause);
            return Err(error(failure));
        }
    };
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
            release_account_operation_gate(durable_gate).await;
            state
                .progressive_sync_cancellations
                .resume(previous_account.id);
            resume_scheduled_mail_rebuild(
                app.clone(),
                state.inner().clone(),
                previous_account.clone(),
            )
            .await;
            return Err(validation_error);
        }
    } else if password_was_supplied && !matches!(account.auth, AccountAuth::Password { .. }) {
        state
            .progressive_sync_cancellations
            .resume(previous_account.id);
        release_account_operation_gate(durable_gate).await;
        resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account.clone())
            .await;
        return Err("OAuth accounts can only be converted after authentication fails".into());
    }
    let namespace_changed = !same_mail_namespace(&previous_account, &account);
    if converts_legacy_oauth && namespace_changed {
        state
            .progressive_sync_cancellations
            .resume(previous_account.id);
        release_account_operation_gate(durable_gate).await;
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
        if let Err(save_error) = durable_gate
            .save_account_with_secret_and_rebuild(
                &account,
                &password_secret_name,
                password.as_deref(),
                None,
                None,
            )
            .await
        {
            release_account_operation_gate(durable_gate).await;
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
        if let Err(release_error) = durable_gate.release().await {
            state.progressive_sync_cancellations.resume(account.id);
            resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
            return Err(error(release_error));
        }
        state.progressive_sync_cancellations.resume(account.id);
        resume_sync_run_after_credential_repair(
            app.clone(),
            state.inner().clone(),
            account.clone(),
        )
        .await;
        if account.enabled {
            state
                .realtime
                .start_account(app.clone(), account.clone())
                .await;
        }
        resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
        return Ok(account);
    }
    let rebuild = namespace_changed.then(|| reset_mail_rebuild_job(account.id));
    if let Err(save_error) = durable_gate
        .save_account_with_secret_and_rebuild(
            &account,
            &password_secret_name,
            password.as_deref(),
            rebuild.as_ref(),
            None,
        )
        .await
    {
        release_account_operation_gate(durable_gate).await;
        state
            .progressive_sync_cancellations
            .resume(previous_account.id);
        if namespace_changed {
            let _ = state.realtime.reconcile(app.clone()).await;
        }
        resume_scheduled_mail_rebuild(app, state.inner().clone(), previous_account).await;
        return Err(error(save_error));
    }
    if let Some(rebuild) = rebuild {
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .insert(account.id, rebuild.into());
    }
    if let Err(release_error) = durable_gate.release().await {
        state.progressive_sync_cancellations.resume(account.id);
        resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
        return Err(error(release_error));
    }
    state.progressive_sync_cancellations.resume(account.id);
    if password_was_supplied {
        resume_sync_run_after_credential_repair(
            app.clone(),
            state.inner().clone(),
            account.clone(),
        )
        .await;
    }
    if !namespace_changed {
        let reconcile = state.realtime.reconcile(app.clone()).await.map_err(error);
        resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
        reconcile?;
        return Ok(account);
    }
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
    // Do not give a rebuild the destructive Remove disposition until SMTP is
    // quiescent. A bounded wait may fail, and its account must keep the
    // retained history checkpoint in that case.
    let durable_gate = state
        .store
        .acquire_account_removal_gate(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account is already being changed or removed".to_owned())?;
    if !state
        .submissions
        .block_and_wait(account_id, ACCOUNT_REMOVAL_SUBMISSION_WAIT)
        .await
    {
        release_account_operation_gate(durable_gate).await;
        state.submissions.unblock(account_id);
        state.mail_rebuild_cancellations.clear_request(account_id);
        return Err(
            "An email is still being submitted. Try removing this account again after it finishes."
                .to_owned(),
        );
    }
    let cross_process_quiet = match wait_for_cross_process_account_operations(
        &state.store,
        account_id,
        ACCOUNT_REMOVAL_SUBMISSION_WAIT,
    )
    .await
    {
        Ok(quiet) => quiet,
        Err(failure) => {
            release_account_operation_gate(durable_gate).await;
            state.submissions.unblock(account_id);
            state.mail_rebuild_cancellations.clear_request(account_id);
            return Err(error(failure));
        }
    };
    if !cross_process_quiet {
        release_account_operation_gate(durable_gate).await;
        state.submissions.unblock(account_id);
        state.mail_rebuild_cancellations.clear_request(account_id);
        return Err(
            "Mail is still finishing an account operation. Try removing this account again after it finishes."
                .to_owned(),
        );
    }
    request_mail_rebuild_cancel(
        state.inner(),
        account_id,
        MailRebuildCancellationDisposition::Remove,
    );
    state.progressive_sync_cancellations.cancel(account_id);
    if !wait_for_progressive_sync_quiescence(
        state.inner(),
        account_id,
        ACCOUNT_REMOVAL_SUBMISSION_WAIT,
    )
    .await
    {
        state.progressive_sync_cancellations.resume(account_id);
        release_account_operation_gate(durable_gate).await;
        abort_account_removal(&app, state.inner(), account_id, None).await;
        return Err(
            "Mail sync is still finishing a provider request. Try removing this account again after it finishes."
                .to_owned(),
        );
    }
    let _operation = state.account_operations.acquire(account_id).await;
    let account = match state.store.account(account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            release_account_operation_gate(durable_gate).await;
            abort_account_removal(&app, state.inner(), account_id, None).await;
            return Err("Account not found".to_owned());
        }
        Err(failure) => {
            release_account_operation_gate(durable_gate).await;
            abort_account_removal(&app, state.inner(), account_id, None).await;
            return Err(error(failure));
        }
    };
    // `stop_account` waits for the watcher task to leave IMAP and complete
    // its current storage call before destructive storage work begins.
    state.realtime.stop_account(account_id).await;
    let credential_name = format!("dev.dakia.mail:{}:{}", account.id, account.auth.username());
    if let Err(failure) = durable_gate
        .delete_account_and_secret(&credential_name)
        .await
    {
        abort_account_removal(&app, state.inner(), account_id, Some(account.clone())).await;
        return Err(error(failure));
    }
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

/// A failed account removal must reopen SMTP and the retained background
/// work. The removal gate is intentionally left closed only after the account
/// row has been deleted, when no future operation can use its credentials.
async fn abort_account_removal(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    account_id: Uuid,
    account: Option<Account>,
) {
    state.submissions.unblock(account_id);
    state.mail_rebuild_cancellations.clear_request(account_id);
    state.progressive_sync_cancellations.resume(account_id);
    if let Some(account) = account {
        state.mail_rebuild_cancellations.clear_request(account.id);
        resume_scheduled_mail_rebuild(app.clone(), state.clone(), account.clone()).await;
        schedule_initial_inbox_sync(app.clone(), state.clone(), account.clone()).await;
        schedule_sent_reconciliation_poll(app.clone(), state.clone(), account.id);
        if let Err(error) = restart_realtime_if_current(app.clone(), state, account.id).await {
            tracing::warn!(account_id = %account.id, error = %error, "could not restart realtime after failed account removal");
        }
    }
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
    // Authenticate against a private, ephemeral credential store before this
    // account is visible to the real catalogue. This keeps a typo, revoked
    // app password, or bad endpoint from leaving a durable but unusable
    // account and a restartable sync job behind.
    let probe_store = Store::in_memory().await.map_err(error)?;
    let secret_name = credential_secret_name(&account);
    probe_store
        .save_account_with_secret(&account, &secret_name, &input.password)
        .await
        .map_err(error)?;
    let probe_mail = MailService::new(probe_store);
    persist_new_account_after_authenticated_probes(
        &state.store,
        &account,
        &secret_name,
        &input.password,
        probe_mail.imap_auth_probe(&account),
        probe_mail.smtp_auth_probe(&account),
    )
    .await
    .map_err(error)?;
    schedule_initial_inbox_sync(app.clone(), state.inner().clone(), account.clone()).await;
    schedule_sent_reconciliation_poll(app, state.inner().clone(), account.id);
    Ok(AccountConnection {
        account,
        reused_existing_account: false,
    })
}

/// Makes the authenticated account visible only after both read-only provider
/// probes have completed. Keeping this boundary separate from the Tauri
/// command gives the failure path one small, testable persistence contract:
/// failed connection setup leaves no account, credential, or SyncRun behind.
async fn persist_new_account_after_authenticated_probes<ImapProbe, SmtpProbe>(
    store: &Store,
    account: &Account,
    secret_name: &str,
    secret: &str,
    imap_probe: ImapProbe,
    smtp_probe: SmtpProbe,
) -> anyhow::Result<SyncRun>
where
    ImapProbe: Future<Output = anyhow::Result<()>>,
    SmtpProbe: Future<Output = anyhow::Result<()>>,
{
    imap_probe.await?;
    smtp_probe.await?;
    store
        .create_account_with_secret_and_initial_sync(account, secret_name, secret)
        .await
}

#[cfg(test)]
mod account_connection_persistence_tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: Uuid::new_v4(),
            email: "new-reader@example.test".into(),
            account_name: "New reader".into(),
            display_name: "New reader".into(),
            provider_id: "custom".into(),
            auth: AccountAuth::Password {
                username: "new-reader@example.test".into(),
            },
            imap_host: "127.0.0.1".into(),
            imap_port: 1,
            imap_security: dakia_core::provider::Security::Tls,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 1,
            smtp_security: dakia_core::provider::Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    fn unavailable_loopback_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    async fn probe_mail(account: &Account, secret: &str) -> MailService {
        let probe_store = Store::in_memory().await.unwrap();
        probe_store
            .save_account_with_secret(account, &credential_secret_name(account), secret)
            .await
            .unwrap();
        MailService::new(probe_store)
    }

    async fn assert_no_durable_connection(store: &Store, account: &Account) {
        assert!(store.account(account.id).await.unwrap().is_none());
        assert!(store.sync_run(account.id).await.unwrap().is_none());
        assert!(store
            .secret(&credential_secret_name(account))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn rejected_loopback_imap_probe_persists_no_account_credential_or_sync_run() {
        let store = Store::in_memory().await.unwrap();
        let mut account = account();
        account.imap_port = unavailable_loopback_port();
        let secret_name = credential_secret_name(&account);
        let probe = probe_mail(&account, "fictional-password").await;

        let failure = persist_new_account_after_authenticated_probes(
            &store,
            &account,
            &secret_name,
            "fictional-password",
            probe.imap_auth_probe(&account),
            async { Ok(()) },
        )
        .await;

        assert!(failure.is_err());
        assert_no_durable_connection(&store, &account).await;
    }

    #[tokio::test]
    async fn rejected_loopback_smtp_probe_persists_no_account_credential_or_sync_run() {
        let store = Store::in_memory().await.unwrap();
        let mut account = account();
        account.smtp_port = unavailable_loopback_port();
        let secret_name = credential_secret_name(&account);
        let probe = probe_mail(&account, "fictional-password").await;

        let failure = persist_new_account_after_authenticated_probes(
            &store,
            &account,
            &secret_name,
            "fictional-password",
            async { Ok(()) },
            probe.smtp_auth_probe(&account),
        )
        .await;

        assert!(failure.is_err());
        assert_no_durable_connection(&store, &account).await;
    }
}

#[tauri::command]
async fn search(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<MailConversationPage, String> {
    let page = state
        .store
        .search_conversation_page(&query)
        .await
        .map_err(error)?;
    if let Some(mailbox) = query.mailbox.filter(|mailbox| !mailbox.trim().is_empty()) {
        let account_ids = if query.account_ids.is_empty() {
            state
                .store
                .accounts()
                .await
                .map_err(error)?
                .into_iter()
                .filter(|account| account.enabled)
                .map(|account| account.id)
                .collect()
        } else {
            query.account_ids
        };
        for account_id in account_ids {
            schedule_folder_history_promotion(
                app.clone(),
                state.inner().clone(),
                account_id,
                mailbox.clone(),
            );
        }
    }
    Ok(page)
}

#[tauri::command]
async fn search_smart_inbox(
    state: State<'_, Arc<AppState>>,
    query: SmartInboxQuery,
) -> Result<SmartInboxPage, String> {
    state.store.search_smart_inbox(&query).await.map_err(error)
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
        state.remote_operation_slots.clone(),
        move |account_id| {
            let state = search_state.clone();
            let text = search_text.clone();
            let mailbox = search_mailbox.clone();
            async move {
                let account = state
                    .store
                    .account(account_id)
                    .await
                    .map_err(error)?
                    .ok_or_else(|| "Account not found".to_owned())?;
                MailService::new(state.store.clone())
                    .search_remote(&account, &text, mailbox.as_deref(), limit)
                    .await
                    .map_err(error)
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
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    starred: bool,
) -> Result<dakia_core::MailSummary, String> {
    let initial = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&initial.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    enabled_account_for_operation(state.inner(), account_id).await?;
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message changed before it could be starred".to_owned())?;
    if message.account_id != initial.account_id {
        return Err("Message changed before it could be starred".to_owned());
    }
    let _journal = operations::enqueue_star_mutation(&state.store, &message, starred, None)
        .await
        .map_err(error)?;
    drop(_operation);
    schedule_message_mutation_drain(app, state.inner().clone(), account_id);
    state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())
}

#[tauri::command]
async fn set_message_read(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    read: bool,
) -> Result<(), String> {
    let initial = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let account_id = Uuid::parse_str(&initial.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    enabled_account_for_operation(state.inner(), account_id).await?;
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message changed before it could be updated".to_owned())?;
    if message.account_id != initial.account_id {
        return Err("Message changed before it could be updated".to_owned());
    }
    let _journal = operations::enqueue_read_mutation(&state.store, &message, read, None)
        .await
        .map_err(error)?;
    drop(_operation);
    schedule_message_mutation_drain(app, state.inner().clone(), account_id);
    Ok(())
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

/// Starts the backend-owned first-Inbox job. The account is already saved and
/// usable when this task begins; its success or failure is represented by the
/// durable SyncRun rather than the account's enabled state.
async fn schedule_initial_inbox_sync(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
) {
    let already_running = !state
        .sync_runs_running
        .lock()
        .expect("sync run reservation lock poisoned")
        .insert(account.id);
    if already_running {
        return;
    }
    let mut cancellation = state.progressive_sync_cancellations.register(account.id);
    tauri::async_runtime::spawn(async move {
        let account_id = account.id;
        if let Err(error) =
            run_initial_inbox_sync(app, state.clone(), account, &mut cancellation).await
        {
            tracing::warn!(account_id = %account_id, error = %error, "initial Inbox sync failed");
        }
        state
            .sync_runs_running
            .lock()
            .expect("sync run reservation lock poisoned")
            .remove(&account_id);
        state.progressive_sync_cancellations.clear(account_id);
    });
}

fn sync_retry_delay(account_id: Uuid, attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(5);
    let seconds = 5_u64.saturating_mul(1_u64 << exponent).min(300);
    // A stable small offset avoids every retained account retrying together
    // after the app reconnects without making the persisted schedule opaque.
    Duration::from_secs(seconds + (account_id.as_u128() as u64 % 5))
}

fn is_persistent_auth_failure(failure: &str) -> bool {
    let failure = failure.to_ascii_lowercase();
    [
        "invalid credentials",
        "authentication failed",
        "authentication rejected",
        "[auth]",
        "[noperm]",
    ]
    .iter()
    .any(|needle| failure.contains(needle))
}

/// Waits for every currently claimed provider action, including mailbox
/// mutations. A settings change must not let an old IMAP action reconcile
/// against the newly saved endpoint configuration after another process has
/// already opened its connection.
async fn wait_for_cross_process_account_operations(
    store: &Store,
    account_id: Uuid,
    timeout: Duration,
) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if store
            .claimed_account_operations(account_id)
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn schedule_sync_retry(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    delay: Duration,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        schedule_initial_inbox_sync(app, state, account).await;
    });
}

enum SyncRunResumeAction {
    Start,
    Schedule(Duration),
    Skip,
}

fn sync_run_resume_action(
    run: Option<&SyncRun>,
    now: chrono::DateTime<chrono::Utc>,
) -> SyncRunResumeAction {
    let Some(run) = run else {
        return SyncRunResumeAction::Start;
    };
    match run.outcome.as_str() {
        "completed" | "paused" => SyncRunResumeAction::Skip,
        "failed" => match run.next_retry_at {
            Some(retry_at) if retry_at > now => {
                let delay = (retry_at - now).to_std().unwrap_or_else(|_| Duration::ZERO);
                SyncRunResumeAction::Schedule(delay)
            }
            _ => SyncRunResumeAction::Start,
        },
        // A process can stop between durable publication and the in-memory
        // worker completing. The run itself is its restart checkpoint.
        "running" => SyncRunResumeAction::Start,
        _ => SyncRunResumeAction::Skip,
    }
}

/// Restores one retained sync run without ignoring its persisted retry
/// deadline. Inbox-ready accounts may have realtime running while this waits
/// to resume their remaining history stages.
async fn resume_durable_sync_run(app: tauri::AppHandle, state: Arc<AppState>, account: Account) {
    let run = match state.store.sync_run(account.id).await {
        Ok(run) => run,
        Err(fetch_error) => {
            tracing::warn!(account_id = %account.id, error = %fetch_error, "could not load durable sync run for resume");
            return;
        }
    };
    match sync_run_resume_action(run.as_ref(), chrono::Utc::now()) {
        SyncRunResumeAction::Start => schedule_initial_inbox_sync(app, state, account).await,
        SyncRunResumeAction::Schedule(delay) => schedule_sync_retry(app, state, account, delay),
        SyncRunResumeAction::Skip => {}
    }
}

/// An explicit credential repair authorizes a previously paused initial run
/// to try again. Other failed runs retain their persisted deadline so merely
/// reopening account settings cannot create a retry storm.
async fn resume_sync_run_after_credential_repair(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
) {
    match state.store.sync_run(account.id).await {
        Ok(Some(run)) if run.outcome == "paused" => {
            if let Err(update_error) = update_sync_run_and_publish(
                &app,
                &state,
                &run,
                SyncRunUpdate {
                    outcome: Some("failed"),
                    next_retry_at: Some(None),
                    error: Some(None),
                    ..SyncRunUpdate::default()
                },
                None,
            )
            .await
            {
                tracing::warn!(account_id = %account.id, error = %update_error, "could not resume paused sync run after credential repair");
                return;
            }
        }
        Ok(_) => {}
        Err(fetch_error) => {
            tracing::warn!(account_id = %account.id, error = %fetch_error, "could not inspect sync run after credential repair");
            return;
        }
    }
    resume_durable_sync_run(app, state, account).await;
}

#[cfg(test)]
mod durable_sync_resume_tests {
    use super::*;

    #[test]
    fn lifecycle_cancel_blocks_a_retry_that_registers_later() {
        let cancellations = ProgressiveSyncCancellations::default();
        let account_id = Uuid::new_v4();
        cancellations.cancel(account_id);
        let receiver = cancellations.register(account_id);
        assert!(progressive_sync_cancelled(&receiver));
        cancellations.clear(account_id);
        // The block remains after the worker registration disappears, until
        // the lifecycle command explicitly resumes this account.
        let receiver = cancellations.register(account_id);
        assert!(progressive_sync_cancelled(&receiver));
        cancellations.clear(account_id);
        cancellations.resume(account_id);
        let receiver = cancellations.register(account_id);
        assert!(!progressive_sync_cancelled(&receiver));
    }

    fn run(
        outcome: &str,
        inbox_ready: bool,
        next_retry_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> SyncRun {
        SyncRun {
            run_id: "run".to_owned(),
            account_id: Uuid::nil(),
            stage: "primary_history".to_owned(),
            inbox_ready,
            primary_complete: false,
            secondary_complete: false,
            deferred_complete: false,
            content_loading: false,
            retry_count: 1,
            outcome: outcome.to_owned(),
            revision: 3,
            next_retry_at,
            error: Some("fixture failure".to_owned()),
        }
    }

    #[test]
    fn retained_future_retry_does_not_restart_early() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::seconds(30);
        match sync_run_resume_action(Some(&run("failed", true, Some(future))), now) {
            SyncRunResumeAction::Schedule(delay) => {
                assert!(delay >= Duration::from_secs(29));
                assert!(delay <= Duration::from_secs(30));
            }
            _ => panic!("future durable retry must remain delayed"),
        }
    }

    #[test]
    fn retained_history_failure_is_resumable_after_its_deadline() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::seconds(1);
        assert!(matches!(
            sync_run_resume_action(Some(&run("failed", true, Some(past))), now),
            SyncRunResumeAction::Start
        ));
    }

    #[test]
    fn paused_auth_failure_requires_explicit_credential_repair() {
        assert!(matches!(
            sync_run_resume_action(Some(&run("paused", false, None)), chrono::Utc::now()),
            SyncRunResumeAction::Skip
        ));
    }

    #[test]
    fn deferred_folder_failure_keeps_sync_incomplete_until_its_earliest_retry() {
        let now = chrono::Utc::now();
        let later = SyncStageDeferred {
            next_retry_at: Some(now + chrono::Duration::seconds(30)),
            user_action_required: false,
        };
        let earlier = SyncStageDeferred {
            next_retry_at: Some(now + chrono::Duration::seconds(10)),
            user_action_required: true,
        };
        let combined = later.merge(earlier);
        assert!(!combined.is_clear());
        assert_eq!(combined.next_retry_at, earlier.next_retry_at);
        assert!(combined.user_action_required);
    }
}

/// Drains one account's durable optimistic mutations outside the Tauri
/// command. Each pass claims at most one operation, so a slow provider cannot
/// hold the lifecycle lock for an unbounded queue. Retry rows retain their
/// local result and keep this single account worker alive until they become
/// due; they must not outlive an arbitrary polling window.
fn schedule_message_mutation_drain(app: tauri::AppHandle, state: Arc<AppState>, account_id: Uuid) {
    let already_running = !state
        .mutation_drains_running
        .lock()
        .expect("mutation drain reservation lock poisoned")
        .insert(account_id);
    if already_running {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            let report = {
                let _operation = state.account_operations.acquire(account_id).await;
                let account = match state.store.account(account_id).await {
                    Ok(Some(account)) if account.enabled => account,
                    Ok(_) => break,
                    Err(error) => {
                        tracing::warn!(account_id = %account_id, error = %error, "could not load account for mutation drain");
                        break;
                    }
                };
                match operations::drain_message_mutations(
                    &state.store,
                    &account,
                    &Uuid::new_v4().to_string(),
                    1,
                )
                .await
                {
                    Ok(report) => report,
                    Err(error) => {
                        tracing::warn!(account_id = %account_id, error = %error, "could not drain durable message mutations");
                        break;
                    }
                }
            };

            for update in &report.updates {
                let _ = app.emit(
                    "mail-operation-updated",
                    serde_json::json!({
                        "operationId": update.operation_id,
                        "accountId": update.account_id,
                        "messageId": update.message_id,
                        "kind": update.kind,
                        "status": update.status,
                        "error": update.error,
                    }),
                );
            }

            if report.claimed > 0 {
                tokio::task::yield_now().await;
                continue;
            }
            let retry_at = match state.store.next_message_mutation_retry_at(account_id).await {
                Ok(retry_at) => retry_at,
                Err(error) => {
                    tracing::warn!(account_id = %account_id, error = %error, "could not read next durable mutation retry deadline");
                    break;
                }
            };
            let Some(retry_at) = retry_at else {
                break;
            };
            let delay = (retry_at - chrono::Utc::now())
                .to_std()
                .unwrap_or_else(|_| Duration::from_secs(1));
            tokio::time::sleep(delay.max(Duration::from_secs(1))).await;
        }
        state
            .mutation_drains_running
            .lock()
            .expect("mutation drain reservation lock poisoned")
            .remove(&account_id);
    });
}

/// A folder selected in navigation receives one bounded historical page ahead
/// of deferred folders. Storage generation checks make a stale promotion a
/// no-op instead of a competing snapshot publication.
fn schedule_folder_history_promotion(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account_id: Uuid,
    local_mailbox: String,
) {
    let reservation = (account_id, local_mailbox.clone());
    let already_running = !state
        .folder_promotions_running
        .lock()
        .expect("folder promotion reservation lock poisoned")
        .insert(reservation.clone());
    if already_running {
        return;
    }
    let cancellation = state.progressive_sync_cancellations.register(account_id);
    tauri::async_runtime::spawn(async move {
        let result: anyhow::Result<()> = async {
            if progressive_sync_cancelled(&cancellation) {
                return Ok(());
            }
            let account = state
                .store
                .account(account_id)
                .await?
                .filter(|account| account.enabled)
                .ok_or_else(|| anyhow::anyhow!("account is unavailable"))?;
            if state
                .store
                .folder_sync_state(account_id, &local_mailbox)
                .await?
                .is_some_and(|folder| folder.headers_complete)
            {
                return Ok(());
            }
            let service = MailService::new(state.store.clone());
            let supported = dakia_core::connection_budget::imap_work(
                dakia_core::connection_budget::ImapPriority::FolderRefresh,
                service.discover_supported_mailboxes(&account),
            )
            .await?;
            if progressive_sync_cancelled(&cancellation) {
                return Ok(());
            }
            let Some(mailbox) = supported
                .into_iter()
                .find(|mailbox| mailbox.local == local_mailbox)
            else {
                return Ok(());
            };
            let promotion_app = app.clone();
            dakia_core::connection_budget::imap_work(
                dakia_core::connection_budget::ImapPriority::FolderRefresh,
                service.backfill_folder_headers_with_progress(
                    &account,
                    &mailbox.local,
                    &mailbox.remote_name,
                    50,
                    move |progress| {
                        let _ = promotion_app.emit(
                            "mail-sync-progress",
                            serde_json::json!({
                                "accountId": account_id,
                                "phase": "promoted_folder",
                                "completed": progress.completed,
                                "total": progress.total,
                            }),
                        );
                    },
                ),
            )
            .await?;
            if progressive_sync_cancelled(&cancellation) {
                return Ok(());
            }
            if let Some(run) = state.store.sync_run(account_id).await? {
                // SyncRun owns the account-wide catalogue revision consumed
                // by windows. A per-folder revision can be lower than a
                // previously published Inbox revision and would be ignored.
                update_sync_run_and_publish(
                    &app,
                    &state,
                    &run,
                    SyncRunUpdate {
                        stage: Some(&run.stage),
                        ..SyncRunUpdate::default()
                    },
                    Some(&local_mailbox),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(account_id = %account_id, mailbox = %local_mailbox, error = %error, "folder history promotion did not complete");
        }
        state
            .folder_promotions_running
            .lock()
            .expect("folder promotion reservation lock poisoned")
            .remove(&reservation);
        state.progressive_sync_cancellations.clear(account_id);
    });
}

fn emit_catalogue_update(
    app: &tauri::AppHandle,
    account_id: Uuid,
    mailbox: Option<&str>,
    revision: u64,
) {
    let _ = app.emit(
        "mail-catalogue-updated",
        serde_json::json!({
            "accountId": account_id,
            "mailbox": mailbox,
            "revision": revision,
        }),
    );
}

async fn update_sync_run_and_publish(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    run: &SyncRun,
    update: SyncRunUpdate<'_>,
    mailbox: Option<&str>,
) -> anyhow::Result<SyncRun> {
    let updated = state.store.update_sync_run(&run.run_id, &update).await?;
    emit_catalogue_update(app, updated.account_id, mailbox, updated.revision);
    Ok(updated)
}

async fn run_initial_inbox_sync(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    requested_account: Account,
    cancellation: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if progressive_sync_cancelled(cancellation) {
        return Ok(());
    }
    let account = match state.store.account(requested_account.id).await? {
        Some(account) if account.enabled => account,
        _ => return Ok(()),
    };
    let run = match state.store.sync_run(account.id).await? {
        Some(run) if run.outcome == "running" => run,
        Some(run)
            if run.outcome == "failed"
                && run.next_retry_at.is_some_and(|at| at > chrono::Utc::now()) =>
        {
            // Startup and a second window may both notice a retained failed
            // run. The durable deadline, rather than process lifetime,
            // controls when it may contact the provider again.
            return Ok(());
        }
        Some(run) if run.outcome == "failed" => {
            state
                .store
                .update_sync_run(
                    &run.run_id,
                    &SyncRunUpdate {
                        outcome: Some("running"),
                        next_retry_at: Some(None),
                        error: Some(None),
                        ..SyncRunUpdate::default()
                    },
                )
                .await?
        }
        Some(_) => return Ok(()),
        None => state.store.create_sync_run(account.id).await?,
    };
    if run.inbox_ready {
        if progressive_sync_cancelled(cancellation) {
            return Ok(());
        }
        restart_realtime_if_current(app.clone(), &state, account.id).await?;
        return run_sync_schedule(&app, &state, &account, run, cancellation).await;
    }
    let progress_app = app.clone();
    let progress_run = run.clone();
    let service = MailService::new(state.store.clone());
    let result = dakia_core::connection_budget::imap_work(
        dakia_core::connection_budget::ImapPriority::FolderRefresh,
        service.initial_inbox_with_progress(&account, 50, move |progress| {
            // Progress callbacks happen before the protocol method returns;
            // durable state changes remain after a committed header page.
            let _ = progress_app.emit(
                "mail-sync-progress",
                serde_json::json!({
                    "accountId": account.id,
                    "runId": progress_run.run_id,
                    "phase": progress.phase,
                    "completed": progress.completed,
                    "total": progress.total,
                }),
            );
        }),
    )
    .await;
    if progressive_sync_cancelled(cancellation) {
        return Ok(());
    }
    match result {
        Ok(_) => {
            let run = update_sync_run_and_publish(
                &app,
                &state,
                &run,
                SyncRunUpdate {
                    stage: Some("primary_history"),
                    inbox_ready: Some(true),
                    ..SyncRunUpdate::default()
                },
                Some("INBOX"),
            )
            .await?;
            restart_realtime_if_current(app.clone(), &state, account.id).await?;
            kick_classification(state.clone());
            match run_sync_schedule(&app, &state, &account, run.clone(), cancellation).await {
                Ok(()) => Ok(()),
                Err(failure) => {
                    let failure_text = failure.to_string();
                    let attempts = run.retry_count.saturating_add(1);
                    let delay = sync_retry_delay(account.id, attempts);
                    let retry_at = chrono::Utc::now()
                        + chrono::Duration::from_std(delay).expect("retry delay fits chrono");
                    update_sync_run_and_publish(
                        &app,
                        &state,
                        &run,
                        SyncRunUpdate {
                            retry_count: Some(attempts),
                            outcome: Some("failed"),
                            next_retry_at: Some(Some(retry_at)),
                            error: Some(Some(&failure_text)),
                            ..SyncRunUpdate::default()
                        },
                        None,
                    )
                    .await?;
                    schedule_sync_retry(app.clone(), state.clone(), account.clone(), delay);
                    Err(failure)
                }
            }
        }
        Err(failure) => {
            let retries = run.retry_count.saturating_add(1);
            let failure_text = failure.to_string();
            let retry = !is_persistent_auth_failure(&failure_text);
            let delay = sync_retry_delay(account.id, retries);
            let retry_at = retry.then(|| {
                chrono::Utc::now()
                    + chrono::Duration::from_std(delay).expect("retry delay fits chrono")
            });
            update_sync_run_and_publish(
                &app,
                &state,
                &run,
                SyncRunUpdate {
                    stage: Some("initial_inbox"),
                    retry_count: Some(retries),
                    outcome: Some(if retry { "failed" } else { "paused" }),
                    next_retry_at: Some(retry_at),
                    error: Some(Some(&failure_text)),
                    ..SyncRunUpdate::default()
                },
                None,
            )
            .await?;
            if retry {
                schedule_sync_retry(app.clone(), state.clone(), account.clone(), delay);
            }
            Err(failure)
        }
    }
}

async fn run_sync_schedule(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    account: &Account,
    mut run: SyncRun,
    cancellation: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if progressive_sync_cancelled(cancellation) {
        return Ok(());
    }
    // LIST is authoritative for provider special-folder names. Never rebuild
    // them from a preset here: a server can expose Sent or Archive under a
    // localized or otherwise noncanonical remote name.
    let service = MailService::new(state.store.clone());
    let supported = dakia_core::connection_budget::imap_work(
        dakia_core::connection_budget::ImapPriority::History,
        service.discover_supported_mailboxes(account),
    )
    .await?;
    if progressive_sync_cancelled(cancellation) {
        return Ok(());
    }
    let primary = supported
        .iter()
        .filter(|mailbox| {
            mailbox.canonical
                && !mailbox.deferred
                && matches!(mailbox.local.as_str(), "INBOX" | "Sent" | "Archive")
        })
        .cloned()
        .collect::<Vec<_>>();
    let secondary = supported
        .iter()
        .filter(|mailbox| {
            mailbox.canonical && !mailbox.deferred && mailbox.local.as_str() == "Drafts"
        })
        .cloned()
        .collect::<Vec<_>>();
    let deferred = supported
        .iter()
        .filter(|mailbox| {
            mailbox.deferred
                || !mailbox.canonical
                || matches!(mailbox.local.as_str(), "Spam" | "Trash")
        })
        .cloned()
        .collect::<Vec<_>>();
    let (next_run, primary_deferred) = backfill_sync_stage(
        app,
        state,
        account,
        run,
        "primary_history",
        &primary,
        cancellation,
    )
    .await?;
    run = next_run;
    run = update_sync_run_and_publish(
        app,
        state,
        &run,
        SyncRunUpdate {
            primary_complete: Some(primary_deferred.is_clear()),
            stage: Some("secondary"),
            ..SyncRunUpdate::default()
        },
        None,
    )
    .await?;
    let (next_run, secondary_deferred) = backfill_sync_stage(
        app,
        state,
        account,
        run,
        "secondary",
        &secondary,
        cancellation,
    )
    .await?;
    run = next_run;
    run = update_sync_run_and_publish(
        app,
        state,
        &run,
        SyncRunUpdate {
            secondary_complete: Some(secondary_deferred.is_clear()),
            stage: Some("deferred"),
            ..SyncRunUpdate::default()
        },
        None,
    )
    .await?;
    let (next_run, deferred_deferred) = backfill_sync_stage(
        app,
        state,
        account,
        run,
        "deferred",
        &deferred,
        cancellation,
    )
    .await?;
    run = next_run;
    let pending = primary_deferred
        .merge(secondary_deferred)
        .merge(deferred_deferred);
    if !pending.is_clear() {
        let mut outstanding = 0_u32;
        for mailbox in &supported {
            outstanding = outstanding.saturating_add(
                state
                    .store
                    .mailbox_header_failure_summary(account.id, &mailbox.local)
                    .await?
                    .outstanding,
            );
        }
        let retry_at = pending.next_retry_at;
        let outcome = if retry_at.is_some() {
            "failed"
        } else {
            "paused"
        };
        let error = if pending.user_action_required {
            "Some message headers need attention before this folder can finish"
        } else {
            "Some message headers are waiting for their retry time"
        };
        update_sync_run_and_publish(
            app,
            state,
            &run,
            SyncRunUpdate {
                deferred_complete: Some(false),
                retry_count: Some(outstanding),
                outcome: Some(outcome),
                next_retry_at: Some(retry_at),
                error: Some(Some(error)),
                ..SyncRunUpdate::default()
            },
            None,
        )
        .await?;
        if let Some(retry_at) = retry_at {
            let delay = (retry_at - chrono::Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            schedule_sync_retry(app.clone(), state.clone(), account.clone(), delay);
        }
        return Ok(());
    }
    update_sync_run_and_publish(
        app,
        state,
        &run,
        SyncRunUpdate {
            deferred_complete: Some(true),
            stage: Some("complete"),
            outcome: Some("completed"),
            ..SyncRunUpdate::default()
        },
        None,
    )
    .await?;
    Ok(())
}

#[derive(Default, Clone, Copy)]
struct SyncStageDeferred {
    next_retry_at: Option<chrono::DateTime<chrono::Utc>>,
    user_action_required: bool,
}

impl SyncStageDeferred {
    fn is_clear(self) -> bool {
        self.next_retry_at.is_none() && !self.user_action_required
    }

    fn merge(self, other: Self) -> Self {
        Self {
            next_retry_at: match (self.next_retry_at, other.next_retry_at) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            },
            user_action_required: self.user_action_required || other.user_action_required,
        }
    }
}

async fn backfill_sync_stage(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    account: &Account,
    mut run: SyncRun,
    stage: &str,
    mailboxes: &[SupportedMailbox],
    cancellation: &mut watch::Receiver<bool>,
) -> anyhow::Result<(SyncRun, SyncStageDeferred)> {
    let mut deferred = SyncStageDeferred::default();
    loop {
        if progressive_sync_cancelled(cancellation) {
            return Ok((run, deferred));
        }
        let mut complete = true;
        let mut made_progress = false;
        for mailbox in mailboxes {
            if progressive_sync_cancelled(cancellation) {
                return Ok((run, deferred));
            }
            if state
                .store
                .folder_sync_state(account.id, &mailbox.local)
                .await?
                .is_some_and(|folder| folder.headers_complete)
            {
                continue;
            }
            let summary = state
                .store
                .mailbox_header_failure_summary(account.id, &mailbox.local)
                .await?;
            if summary.user_action_required > 0
                || summary
                    .next_retry_at
                    .is_some_and(|retry_at| retry_at > chrono::Utc::now())
            {
                complete = false;
                deferred = deferred.merge(SyncStageDeferred {
                    next_retry_at: summary.next_retry_at,
                    user_action_required: summary.user_action_required > 0,
                });
                continue;
            }
            let revision_before = state
                .store
                .folder_sync_state(account.id, &mailbox.local)
                .await?
                .map(|folder| folder.revision);
            let progress_app = app.clone();
            let account_id = account.id;
            let run_id = run.run_id.clone();
            let service = MailService::new(state.store.clone());
            let priority = if stage == "deferred" {
                dakia_core::connection_budget::ImapPriority::Deferred
            } else {
                dakia_core::connection_budget::ImapPriority::History
            };
            dakia_core::connection_budget::imap_work(
                priority,
                service.backfill_folder_headers_with_progress(
                    account,
                    &mailbox.local,
                    &mailbox.remote_name,
                    50,
                    move |progress| {
                        let _ = progress_app.emit(
                            "mail-sync-progress",
                            serde_json::json!({
                                "accountId": account_id,
                                "runId": run_id,
                                "phase": progress.phase,
                                "completed": progress.completed,
                                "total": progress.total,
                            }),
                        );
                    },
                ),
            )
            .await?;
            if progressive_sync_cancelled(cancellation) {
                return Ok((run, deferred));
            }
            let folder = state
                .store
                .folder_sync_state(account.id, &mailbox.local)
                .await?
                .ok_or_else(|| anyhow::anyhow!("folder sync state disappeared"))?;
            complete &= folder.headers_complete;
            made_progress |= revision_before.is_none_or(|before| folder.revision > before);
            let summary = state
                .store
                .mailbox_header_failure_summary(account.id, &mailbox.local)
                .await?;
            if !folder.headers_complete
                && (summary.user_action_required > 0
                    || summary
                        .next_retry_at
                        .is_some_and(|retry_at| retry_at > chrono::Utc::now()))
            {
                deferred = deferred.merge(SyncStageDeferred {
                    next_retry_at: summary.next_retry_at,
                    user_action_required: summary.user_action_required > 0,
                });
            }
            run = update_sync_run_and_publish(
                app,
                state,
                &run,
                SyncRunUpdate {
                    stage: Some(stage),
                    ..SyncRunUpdate::default()
                },
                Some(&mailbox.local),
            )
            .await?;
            // Give request-triggered opens and SMTP work a chance to claim
            // their own connections between bounded header batches.
            tokio::task::yield_now().await;
        }
        if complete {
            return Ok((run, deferred));
        }
        if !made_progress {
            return Ok((run, deferred));
        }
    }
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

async fn resume_scheduled_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
) {
    state.mail_rebuild_cancellations.clear_request(account.id);
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
    // Register before provider work. Account updates/removal can request
    // cancellation without waiting for a historical IMAP operation, leaving
    // SMTP and the rest of the lifecycle responsive during backfill.
    let cancel_receiver = state.mail_rebuild_cancellations.register(account.id);
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
    // The durable job is the source of truth while this worker is active.
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

/// Reloadable, backend-owned synchronization state. Windows must treat this
/// SQLite result as authoritative after a reconnect or a missed native event.
#[tauri::command]
async fn mail_sync_status(state: State<'_, Arc<AppState>>) -> Result<Vec<SyncRun>, String> {
    state.store.sync_runs().await.map_err(error)
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutgoingSubmission {
    operation_id: String,
    status: String,
    response: Option<String>,
    persistence_warning: bool,
}

enum DesktopSubmissionClaim {
    Claimed(dakia_core::storage::OperationJournalEntry),
    Finished(OutgoingSubmission),
}

fn durable_outgoing_submission_outcome(
    operation: &dakia_core::storage::OperationJournalEntry,
) -> Result<Option<OutgoingSubmission>, String> {
    let accepted = operation.smtp_accepted_at.is_some();
    let outcome = match operation.state.as_str() {
        "queued" | "submitting" => return Ok(None),
        "retry" if accepted => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "sent_copy_pending".to_owned(),
            response: None,
            persistence_warning: operation.error.is_some(),
        },
        "retry" => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "queued".to_owned(),
            response: None,
            persistence_warning: operation.error.is_some(),
        },
        "sent_copy_pending" => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "sent_copy_pending".to_owned(),
            response: None,
            persistence_warning: operation.error.is_some(),
        },
        "accepted" | "completed" if accepted => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "accepted".to_owned(),
            response: None,
            persistence_warning: operation.error.is_some(),
        },
        "accepted" | "completed" => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "uncertain".to_owned(),
            response: None,
            persistence_warning: true,
        },
        "uncertain" => OutgoingSubmission {
            operation_id: operation.operation_id.clone(),
            status: "uncertain".to_owned(),
            response: None,
            persistence_warning: false,
        },
        "rejected" | "permanent_failed" => {
            return Err(format!(
                "durable SMTP submission was rejected; do not send again. Operation: {}{}",
                operation.operation_id,
                operation
                    .error
                    .as_deref()
                    .map(|failure| format!("; {failure}"))
                    .unwrap_or_default()
            ));
        }
        state => {
            return Err(format!(
                "outgoing submission journal entry has an unexpected state: {state}"
            ));
        }
    };
    Ok(Some(outcome))
}

async fn wait_for_owned_outgoing_submission_claim(
    store: &Store,
    account_id: Uuid,
    operation_id: &str,
    claim_owner: &str,
) -> Result<DesktopSubmissionClaim, String> {
    let deadline = Instant::now() + ACCOUNT_REMOVAL_SUBMISSION_WAIT;
    let mut next_recovery = Instant::now();
    loop {
        let now = Instant::now();
        if now >= next_recovery {
            // Recover only expired claims. A separate desktop or CLI that is
            // still heartbeating its SMTP operation must retain its lease.
            store
                .mark_interrupted_operations_uncertain(account_id)
                .await
                .map_err(error)?;
            next_recovery = now + Duration::from_secs(1);
        }
        let operation = store
            .operation_journal_entry(operation_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Durable SMTP submission disappeared".to_owned())?;
        if let Some(outcome) = durable_outgoing_submission_outcome(&operation)? {
            return Ok(DesktopSubmissionClaim::Finished(outcome));
        }
        if operation.state == "queued" {
            if let Some(claimed) = store
                .claim_operation_by_id(operation_id, claim_owner)
                .await
                .map_err(error)?
            {
                if claimed.kind != "smtp_submission" || claimed.operation_id != operation_id {
                    return Err(
                        "outgoing submission journal entry has an unexpected claim".to_owned()
                    );
                }
                return Ok(DesktopSubmissionClaim::Claimed(claimed));
            }
        }
        if Instant::now() >= deadline {
            return Ok(DesktopSubmissionClaim::Finished(OutgoingSubmission {
                operation_id: operation_id.to_owned(),
                status: "queued".to_owned(),
                response: None,
                persistence_warning: true,
            }));
        }
        tokio::time::sleep(OUTGOING_SUBMISSION_CLAIM_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod outgoing_submission_tests {
    use super::*;

    async fn account_with_store() -> (Store, Account) {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = Account {
            id: Uuid::new_v4(),
            email: "outgoing@example.test".to_owned(),
            account_name: "Outgoing".to_owned(),
            display_name: "Outgoing".to_owned(),
            provider_id: "fastmail".to_owned(),
            auth: AccountAuth::Password {
                username: "outgoing@example.test".to_owned(),
            },
            imap_host: "imap.example.test".to_owned(),
            imap_port: 993,
            imap_security: dakia_core::provider::Security::Tls,
            smtp_host: "smtp.example.test".to_owned(),
            smtp_port: 465,
            smtp_security: dakia_core::provider::Security::Tls,
            archive_mailbox: "Archive".to_owned(),
            spam_mailbox: "Spam".to_owned(),
            enabled: true,
            created_at: chrono::Utc::now(),
        };
        store.save_account(&account).await.expect("save account");
        (store, account)
    }

    fn operation(
        state: &str,
        accepted_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> dakia_core::storage::OperationJournalEntry {
        let now = chrono::Utc::now();
        dakia_core::storage::OperationJournalEntry {
            operation_id: "outgoing-op".to_owned(),
            account_id: Uuid::nil().to_string(),
            mailbox: None,
            uid: None,
            uid_validity: None,
            message_id: None,
            kind: "smtp_submission".to_owned(),
            payload_json: "{}".to_owned(),
            local_version: 0,
            dependency_id: None,
            state: state.to_owned(),
            outcome: Some(
                "an implementation detail that must not change delivery state".to_owned(),
            ),
            attempts: 1,
            next_retry_at: None,
            error: Some("Sent copy needs attention".to_owned()),
            smtp_accepted_at: accepted_at,
            claim_owner: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn accepted_marker_drives_recovered_delivery_state() {
        let accepted_operation = operation("uncertain", Some(chrono::Utc::now()));
        assert!(unresolved_delivery_was_accepted(&accepted_operation));

        let without_marker = operation("uncertain", None);
        assert!(!unresolved_delivery_was_accepted(&without_marker));
    }

    #[test]
    fn accepted_sent_copy_retry_never_becomes_delivery_uncertain() {
        let operation = operation("retry", Some(chrono::Utc::now()));
        let outcome = durable_outgoing_submission_outcome(&operation)
            .expect("valid durable operation")
            .expect("finished outcome");
        assert_eq!(outcome.status, "sent_copy_pending");
        assert_ne!(outcome.status, "uncertain");
    }

    #[test]
    fn only_confirmed_sent_copies_request_a_catalogue_refresh() {
        // This routing drives the real bounded primary-mailbox fetch in
        // `reconcile_pending_sent_copies_inner`. Before this path existed, a
        // completed APPEND was terminal journal state and Sent stayed absent
        // locally until the periodic warmer happened to run.
        assert!(completed_sent_copy_requires_catalogue_refresh(
            "completed",
            Some("sent_copy_saved")
        ));
        assert!(completed_sent_copy_requires_catalogue_refresh(
            "completed",
            Some("provider_sent_reconciled")
        ));
        assert!(!completed_sent_copy_requires_catalogue_refresh(
            "uncertain",
            Some("smtp_accepted_sent_copy_uncertain")
        ));
        assert!(!completed_sent_copy_requires_catalogue_refresh(
            "retry",
            Some("sent_copy_retry_scheduled")
        ));
    }

    #[tokio::test]
    async fn queued_submission_claims_only_the_callers_durable_draft() {
        let (store, account) = account_with_store().await;
        let active = store
            .enqueue_smtp_submission_and_claim(account.id, r#"{"draft":"active"}"#, "active")
            .await
            .expect("stage active submission");
        let queued = store
            .enqueue_smtp_submission_and_claim(account.id, r#"{"draft":"queued"}"#, "queued")
            .await
            .expect("stage queued submission");
        assert_eq!(active.state, "submitting");
        assert_eq!(queued.state, "queued");

        store
            .complete_claimed_operation(
                &active.operation_id,
                "active",
                "rejected",
                Some("rejected_before_acceptance"),
                Some("fixture complete"),
                None,
            )
            .await
            .expect("finish predecessor");

        match wait_for_owned_outgoing_submission_claim(
            &store,
            account.id,
            &queued.operation_id,
            "queued",
        )
        .await
        .expect("wait for own submission")
        {
            DesktopSubmissionClaim::Claimed(claimed) => {
                assert_eq!(claimed.operation_id, queued.operation_id);
                assert_eq!(claimed.state, "submitting");
            }
            DesktopSubmissionClaim::Finished(_) => {
                panic!("the caller's queued submission must be claimed by its owner")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableOutgoingPayload {
    draft: ComposeMessage,
    prepared: PreparedOutgoingMessage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnresolvedMailOperation {
    operation_id: String,
    account_id: String,
    message_id: Option<String>,
    kind: String,
    status: String,
    outcome: Option<String>,
    delivery_accepted: bool,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn unresolved_delivery_was_accepted(
    operation: &dakia_core::storage::OperationJournalEntry,
) -> bool {
    operation.kind == "smtp_submission" && operation.smtp_accepted_at.is_some()
}

#[tauri::command]
async fn mail_unresolved_operations(
    state: State<'_, Arc<AppState>>,
    account_ids: Option<Vec<Uuid>>,
) -> Result<Vec<UnresolvedMailOperation>, String> {
    let account_ids = match account_ids {
        Some(account_ids) => account_ids,
        None => state
            .store
            .accounts()
            .await
            .map_err(error)?
            .into_iter()
            .map(|account| account.id)
            .collect(),
    };
    let mut operations = Vec::new();
    for account_id in account_ids {
        for operation in state
            .store
            .unresolved_operations(account_id)
            .await
            .map_err(error)?
        {
            let delivery_accepted = unresolved_delivery_was_accepted(&operation);
            operations.push(UnresolvedMailOperation {
                operation_id: operation.operation_id,
                account_id: operation.account_id,
                message_id: operation.message_id,
                kind: operation.kind,
                status: operation.state,
                outcome: operation.outcome,
                delivery_accepted,
                error: operation.error,
                created_at: operation.created_at,
            });
        }
    }
    operations.sort_by_key(|operation| operation.created_at);
    Ok(operations)
}

/// Returns a saved submission for a recovery-only compose view. It never
/// returns ordinary drafts and the caller cannot use this command to resend.
#[tauri::command]
async fn outgoing_operation_draft(
    state: State<'_, Arc<AppState>>,
    operation_id: String,
) -> Result<ComposeMessage, String> {
    let operation = state
        .store
        .operation_journal_entry(&operation_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Outgoing operation not found".to_owned())?;
    if operation.kind != "smtp_submission"
        || !matches!(operation.state.as_str(), "uncertain" | "accepted")
    {
        return Err("This operation does not have a recoverable outgoing draft".to_owned());
    }
    serde_json::from_str::<DurableOutgoingPayload>(&operation.payload_json)
        .map(|payload| payload.draft)
        .map_err(error)
}

fn sent_reconcile_retry_delay(operation_id: &str, attempts: i64) -> Duration {
    let exponent = u32::try_from(attempts.clamp(0, 6)).unwrap_or(6);
    let seconds = 5_u64.saturating_mul(1_u64 << exponent).min(300);
    let jitter = operation_id
        .bytes()
        .fold(0_u64, |total, byte| total.saturating_add(u64::from(byte)))
        % 1_000;
    Duration::from_secs(seconds) + Duration::from_millis(jitter)
}

fn sent_reconcile_failure_is_authentication(error: &str) -> bool {
    is_persistent_auth_failure(error) || error.to_ascii_lowercase().contains("oauth authentication")
}

fn sent_reconcile_failure_is_retryable(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "timeout",
        "timed out",
        "temporary",
        "connection reset",
        "connection refused",
        "network is unreachable",
        "broken pipe",
        "rate limit",
        "throttl",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

/// One account timer services every due Sent-copy retry. Individual
/// journal rows carry their own jittered deadline, so a burst of failures
/// cannot create a burst of IMAP timers or reconnects.
fn schedule_sent_reconciliation_retry(
    app: Option<tauri::AppHandle>,
    state: Arc<AppState>,
    account_id: Uuid,
) {
    let already_running = !state
        .sent_reconcile_retry_running
        .lock()
        .expect("Sent reconciliation retry reservation lock poisoned")
        .insert(account_id);
    if already_running {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // Each row owns its deadline. A restart can arm this timer for a
        // future row before any IMAP command is due.
        loop {
            let retry_at = match state
                .store
                .next_sent_reconciliation_retry_at(account_id)
                .await
            {
                Ok(retry_at) => retry_at,
                Err(error) => {
                    tracing::warn!(account_id = %account_id, error = %error, "could not read next Sent reconciliation deadline");
                    break;
                }
            };
            let Some(retry_at) = retry_at else { break };
            let delay = (retry_at - chrono::Utc::now())
                .to_std()
                .unwrap_or_else(|_| Duration::ZERO);
            tokio::time::sleep(delay).await;
            reconcile_pending_sent_copies_inner(app.clone(), state.clone(), account_id).await;
        }
        state
            .sent_reconcile_retry_running
            .lock()
            .expect("Sent reconciliation retry reservation lock poisoned")
            .remove(&account_id);
    });
}

/// A lightweight durable journal poll observes Sent-copy work submitted by a
/// concurrent CLI process. Local Tauri events cannot wake this process for a
/// SQLite write made elsewhere.
fn schedule_sent_reconciliation_poll(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account_id: Uuid,
) {
    let already_running = !state
        .sent_reconcile_poll_running
        .lock()
        .expect("Sent reconciliation poll reservation lock poisoned")
        .insert(account_id);
    if already_running {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            match state.store.account(account_id).await {
                Ok(Some(account)) if account.enabled => {
                    if let Err(error) =
                        operations::recover_interrupted_account_operations(&state.store, account_id)
                            .await
                    {
                        tracing::warn!(account_id = %account_id, error = %error, "could not recover expired cross-process operation claims");
                    }
                    schedule_message_mutation_drain(app.clone(), state.clone(), account_id);
                    reconcile_pending_sent_copies(Some(app.clone()), state.clone(), account_id);
                }
                Ok(_) => break,
                Err(error) => {
                    tracing::warn!(account_id = %account_id, error = %error, "could not poll cross-process Sent reconciliation work");
                    break;
                }
            }
        }
        state
            .sent_reconcile_poll_running
            .lock()
            .expect("Sent reconciliation poll reservation lock poisoned")
            .remove(&account_id);
    });
}

fn reconcile_pending_sent_copies(
    app: Option<tauri::AppHandle>,
    state: Arc<AppState>,
    account_id: Uuid,
) {
    tauri::async_runtime::spawn(async move {
        reconcile_pending_sent_copies_inner(app, state, account_id).await;
    });
}

/// Publish the provider copy into the local Sent catalogue as soon as its
/// durable Sent-copy operation completes. This does not depend on Inbox or
/// Archive being available, and it can establish Sent for a fresh account.
async fn refresh_recent_sent_catalogue_after_sent_copy(
    app: Option<&tauri::AppHandle>,
    state: &Arc<AppState>,
    account: &Account,
) -> anyhow::Result<usize> {
    let refreshed = dakia_core::connection_budget::imap_work(
        dakia_core::connection_budget::ImapPriority::Realtime,
        MailService::new(state.store.clone()).refresh_recent_sent_mailbox(
            account,
            chrono::Utc::now() - chrono::Duration::days(SENT_COPY_CATALOGUE_REFRESH_LOOKBACK_DAYS),
            SENT_COPY_CATALOGUE_REFRESH_LIMIT,
        ),
    )
    .await?;
    if !refreshed.is_empty() {
        if let Some(app) = app {
            // `mail-changed` is the existing committed-catalogue wake-up for
            // all windows. SyncRun revisions are terminal after a completed
            // history run, so inventing a revision here would let clients
            // suppress a real later publication.
            let _ = app.emit(
                "mail-changed",
                serde_json::json!({ "accountId": account.id }),
            );
        }
    }
    Ok(refreshed.len())
}

fn completed_sent_copy_requires_catalogue_refresh(state: &str, outcome: Option<&str>) -> bool {
    state == "completed"
        && matches!(
            outcome,
            Some("sent_copy_saved" | "provider_sent_reconciled")
        )
}

async fn reconcile_pending_sent_copies_inner(
    app: Option<tauri::AppHandle>,
    state: Arc<AppState>,
    account_id: Uuid,
) {
    let _submission = match state.submissions.acquire(account_id).await {
        Ok(submission) => submission,
        Err(_) => return,
    };
    let account = match state.store.account(account_id).await {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(account_id = %account_id, error = %error, "could not load account for Sent-copy reconciliation");
            return;
        }
    };
    let provider_reconciliations = match state
        .store
        .provider_sent_reconciliation_operations(account_id)
        .await
    {
        Ok(operations) => operations,
        Err(error) => {
            tracing::warn!(account_id = %account_id, error = %error, "could not load provider Sent reconciliations");
            Vec::new()
        }
    };
    let mut provider_retry_scheduled = false;
    let mut catalogue_refresh_needed = false;
    for operation in provider_reconciliations {
        let owner = Uuid::new_v4().to_string();
        let Some(claimed) = (match state
            .store
            .claim_provider_sent_operation(&operation.operation_id, &owner)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(operation_id = %operation.operation_id, error = %error, "could not claim provider Sent reconciliation");
                continue;
            }
        }) else {
            continue;
        };
        let _claim_heartbeat = ClaimHeartbeat::start(
            state.store.clone(),
            claimed.operation_id.clone(),
            owner.clone(),
        );
        let payload: DurableOutgoingPayload = match serde_json::from_str(&claimed.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = state
                    .store
                    .complete_claimed_operation(
                        &claimed.operation_id,
                        &owner,
                        "uncertain",
                        Some("invalid_prepared_outgoing_payload"),
                        Some(&error.to_string()),
                        None,
                    )
                    .await;
                continue;
            }
        };
        let (completion, retry_this_operation) = match dakia_core::connection_budget::imap_work(
            dakia_core::connection_budget::ImapPriority::Realtime,
            MailService::new(state.store.clone())
                .reconcile_provider_sent_copy(&account, &payload.prepared),
        )
        .await
        {
            Ok(SentCopyPresence::Present) => {
                (("completed", Some("provider_sent_reconciled"), None), false)
            }
            Ok(SentCopyPresence::Absent) => {
                provider_retry_scheduled = true;
                (
                    (
                        "accepted",
                        Some("provider_sent_reconciliation"),
                        Some("waiting for the provider Sent copy".to_owned()),
                    ),
                    true,
                )
            }
            Err(error) => {
                let error = error.to_string().chars().take(512).collect::<String>();
                let retry = !sent_reconcile_failure_is_authentication(&error)
                    && sent_reconcile_failure_is_retryable(&error);
                provider_retry_scheduled |= retry;
                (
                    (
                        if retry { "accepted" } else { "uncertain" },
                        Some(if retry {
                            "provider_sent_reconciliation"
                        } else {
                            "provider_sent_reconciliation_uncertain"
                        }),
                        Some(error),
                    ),
                    retry,
                )
            }
        };
        let retry_at = retry_this_operation.then(|| {
            chrono::Utc::now()
                + chrono::Duration::from_std(sent_reconcile_retry_delay(
                    &claimed.operation_id,
                    claimed.attempts,
                ))
                .expect("retry delay fits chrono")
        });
        let provider_copy_confirmed =
            completed_sent_copy_requires_catalogue_refresh(completion.0, completion.1);
        if let Err(error) = state
            .store
            .complete_claimed_operation(
                &claimed.operation_id,
                &owner,
                completion.0,
                completion.1,
                completion.2.as_deref(),
                retry_at,
            )
            .await
        {
            tracing::warn!(operation_id = %claimed.operation_id, error = %error, "could not record provider Sent reconciliation outcome");
        } else if provider_copy_confirmed {
            catalogue_refresh_needed = true;
        }
    }
    let pending = match state.store.pending_operations(account_id).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(account_id = %account_id, error = %error, "could not load pending Sent-copy operations");
            return;
        }
    };
    let mut accepted_submission_recovered = false;
    for operation in pending
        .iter()
        .filter(|operation| operation.kind == "smtp_submission" && operation.state == "queued")
    {
        let owner = Uuid::new_v4().to_string();
        let Some(claimed) = (match state
            .store
            .claim_operation_by_id(&operation.operation_id, &owner)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(operation_id = %operation.operation_id, error = %error, "could not claim queued SMTP submission");
                continue;
            }
        }) else {
            continue;
        };
        let _claim_heartbeat = ClaimHeartbeat::start(
            state.store.clone(),
            claimed.operation_id.clone(),
            owner.clone(),
        );
        let payload: DurableOutgoingPayload = match serde_json::from_str(&claimed.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = state
                    .store
                    .complete_claimed_operation(
                        &claimed.operation_id,
                        &owner,
                        "permanent_failed",
                        Some("invalid_prepared_outgoing_payload"),
                        Some(&error.to_string()),
                        None,
                    )
                    .await;
                continue;
            }
        };
        let completion = match MailService::new(state.store.clone())
            .submit_prepared_smtp(&account, &payload.prepared)
            .await
        {
            Ok(SendOutcome::Accepted { sent_copy, .. }) => {
                accepted_submission_recovered = true;
                match sent_copy {
                    SentCopyStatus::ProviderManaged => {
                        ("accepted", Some("provider_sent_reconciliation"), None)
                    }
                    SentCopyStatus::Pending => ("sent_copy_pending", Some("smtp_accepted"), None),
                }
            }
            Ok(SendOutcome::DeliveryUncertain) => {
                ("uncertain", Some("smtp_delivery_uncertain"), None)
            }
            Err(error) => (
                "rejected",
                Some("rejected_before_acceptance"),
                Some(error.to_string()),
            ),
        };
        if let Err(error) = state
            .store
            .complete_claimed_operation(
                &claimed.operation_id,
                &owner,
                completion.0,
                completion.1,
                completion.2.as_deref(),
                None,
            )
            .await
        {
            tracing::warn!(operation_id = %claimed.operation_id, error = %error, "could not record recovered SMTP submission outcome");
        }
    }
    let mut sent_copy_retry_scheduled = false;
    for operation in pending.into_iter().filter(|operation| {
        operation.kind == "smtp_submission"
            && (operation.state == "sent_copy_pending"
                || (operation.state == "retry"
                    && operation.outcome.as_deref() == Some("sent_copy_retry_scheduled")))
    }) {
        let owner = Uuid::new_v4().to_string();
        let Some(claimed) = (match state
            .store
            .claim_sent_copy_operation(&operation.operation_id, &owner)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(operation_id = %operation.operation_id, error = %error, "could not claim Sent-copy operation");
                continue;
            }
        }) else {
            continue;
        };
        let _claim_heartbeat = ClaimHeartbeat::start(
            state.store.clone(),
            claimed.operation_id.clone(),
            owner.clone(),
        );
        let payload: DurableOutgoingPayload = match serde_json::from_str(&claimed.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = state
                    .store
                    .complete_claimed_operation(
                        &claimed.operation_id,
                        &owner,
                        "permanent_failed",
                        Some("invalid_prepared_outgoing_payload"),
                        Some(&error.to_string()),
                        None,
                    )
                    .await;
                continue;
            }
        };
        let result = dakia_core::connection_budget::imap_work(
            dakia_core::connection_budget::ImapPriority::Realtime,
            MailService::new(state.store.clone())
                .save_pending_sent_copy(&account, &payload.prepared),
        )
        .await;
        let completion = match result {
            Ok(SentCopyOutcome::Saved) => ("completed", Some("sent_copy_saved"), None, None),
            Ok(SentCopyOutcome::Uncertain) => (
                "uncertain",
                Some("smtp_accepted_sent_copy_uncertain"),
                None,
                None,
            ),
            Err(error) => {
                let error = error.to_string();
                if sent_reconcile_failure_is_authentication(&error) {
                    (
                        "uncertain",
                        Some("sent_copy_authentication_required"),
                        Some(error),
                        None,
                    )
                } else if sent_reconcile_failure_is_retryable(&error) {
                    sent_copy_retry_scheduled = true;
                    (
                        "retry",
                        Some("sent_copy_retry_scheduled"),
                        Some(error),
                        Some(
                            chrono::Utc::now()
                                + chrono::Duration::from_std(sent_reconcile_retry_delay(
                                    &claimed.operation_id,
                                    claimed.attempts,
                                ))
                                .expect("retry delay fits chrono"),
                        ),
                    )
                } else {
                    (
                        "uncertain",
                        Some("sent_copy_reconciliation_uncertain"),
                        Some(error),
                        None,
                    )
                }
            }
        };
        let sent_copy_saved =
            completed_sent_copy_requires_catalogue_refresh(completion.0, completion.1);
        if let Err(error) = state
            .store
            .complete_claimed_operation(
                &claimed.operation_id,
                &owner,
                completion.0,
                completion.1,
                completion.2.as_deref(),
                completion.3,
            )
            .await
        {
            tracing::warn!(operation_id = %claimed.operation_id, error = %error, "could not record Sent-copy reconciliation outcome");
        } else if sent_copy_saved {
            catalogue_refresh_needed = true;
        }
    }
    if catalogue_refresh_needed {
        if let Err(error) =
            refresh_recent_sent_catalogue_after_sent_copy(app.as_ref(), &state, &account).await
        {
            // The Sent copy is already durably complete. Keep it complete and
            // let the ordinary bounded primary refresh retry catalogue
            // visibility, rather than risking a second APPEND.
            tracing::warn!(account_id = %account_id, error = %error, "could not refresh local Sent catalogue after a saved copy");
        }
    }
    if provider_retry_scheduled
        || sent_copy_retry_scheduled
        || state
            .store
            .next_sent_reconciliation_retry_at(account_id)
            .await
            .ok()
            .flatten()
            .is_some()
    {
        schedule_sent_reconciliation_retry(app.clone(), state.clone(), account_id);
    }
    if accepted_submission_recovered {
        reconcile_pending_sent_copies(app, state.clone(), account_id);
    }
}

async fn submit_outgoing_message(
    app: Option<&tauri::AppHandle>,
    state: &Arc<AppState>,
    draft: &ComposeMessage,
) -> Result<OutgoingSubmission, String> {
    let _submission = state
        .submissions
        .acquire(draft.account_id)
        .await
        .map_err(|_| "Account is being removed".to_owned())?;
    let account = enabled_account_for_operation(state, draft.account_id).await?;
    let service = MailService::new(state.store.clone());
    let prepared = service
        .prepare_outgoing_message(&account, draft)
        .map_err(error)?;
    let payload = serde_json::to_string(&DurableOutgoingPayload {
        draft: draft.clone(),
        prepared: prepared.clone(),
    })
    .map_err(error)?;
    // The complete, final composer payload is durable before SMTP starts.
    // A submission that dies after DATA remains fenced as uncertain rather
    // than becoming a candidate for automatic replay on restart.
    let claim_owner = Uuid::new_v4().to_string();
    let claimed = state
        .store
        .enqueue_smtp_submission_and_claim(account.id, &payload, &claim_owner)
        .await
        .map_err(error)?;
    let operation_id = claimed.operation_id.clone();
    let claimed = match claimed.state.as_str() {
        "submitting" => claimed,
        "queued" => match wait_for_owned_outgoing_submission_claim(
            &state.store,
            account.id,
            &operation_id,
            &claim_owner,
        )
        .await?
        {
            DesktopSubmissionClaim::Claimed(claimed) => claimed,
            DesktopSubmissionClaim::Finished(outcome) => return Ok(outcome),
        },
        status => {
            return Err(format!(
                "outgoing submission journal entry has an unexpected state: {status}"
            ));
        }
    };
    if claimed.operation_id != operation_id || claimed.state != "submitting" {
        return Err("outgoing submission journal entry has an unexpected claim".to_owned());
    }
    let _lease_heartbeat = ClaimHeartbeat::start(
        state.store.clone(),
        claimed.operation_id.clone(),
        claim_owner.clone(),
    );
    match service.submit_prepared_smtp(&account, &prepared).await {
        Ok(SendOutcome::Accepted {
            response,
            sent_copy,
        }) => {
            let (status, outcome) = match sent_copy {
                SentCopyStatus::ProviderManaged => ("accepted", "provider_sent_reconciliation"),
                SentCopyStatus::Pending => ("sent_copy_pending", "smtp_accepted"),
            };
            let persisted = match state
                .store
                .complete_claimed_operation(
                    &claimed.operation_id,
                    &claim_owner,
                    status,
                    Some(outcome),
                    None,
                    None,
                )
                .await
            {
                Ok(()) => true,
                Err(error) => {
                    // SMTP already accepted the message. Returning an
                    // ordinary error here would invite a duplicate send.
                    // Keep the durable row fenced as `submitting`; startup
                    // will expose it as uncertainty before any worker runs.
                    tracing::error!(
                        operation_id = %claimed.operation_id,
                        error = %error,
                        "could not persist SMTP acceptance outcome"
                    );
                    false
                }
            };
            if persisted {
                reconcile_pending_sent_copies(app.cloned(), state.clone(), account.id);
            }
            Ok(OutgoingSubmission {
                operation_id: claimed.operation_id,
                status: if persisted {
                    status.to_owned()
                } else {
                    // SMTP has already returned 250. The local journal needs
                    // attention, but delivery is not uncertain and compose
                    // must never encourage a duplicate send.
                    "accepted".to_owned()
                },
                response: Some(response),
                persistence_warning: !persisted,
            })
        }
        Ok(SendOutcome::DeliveryUncertain) => {
            if let Err(error) = state
                .store
                .complete_claimed_operation(
                    &claimed.operation_id,
                    &claim_owner,
                    "uncertain",
                    Some("smtp_delivery_uncertain"),
                    None,
                    None,
                )
                .await
            {
                tracing::error!(
                    operation_id = %claimed.operation_id,
                    error = %error,
                    "could not persist uncertain SMTP outcome"
                );
            }
            Ok(OutgoingSubmission {
                operation_id: claimed.operation_id,
                status: "uncertain".to_owned(),
                response: None,
                persistence_warning: false,
            })
        }
        Err(failure) => {
            let failure_text = failure.to_string();
            state
                .store
                .complete_claimed_operation(
                    &claimed.operation_id,
                    &claim_owner,
                    "rejected",
                    Some("rejected_before_acceptance"),
                    Some(&failure_text),
                    None,
                )
                .await
                .map_err(error)?;
            Err(error(failure))
        }
    }
}

#[tauri::command]
async fn send_message_outcome(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    draft: ComposeMessage,
) -> Result<OutgoingSubmission, String> {
    submit_outgoing_message(Some(&app), state.inner(), &draft).await
}

/// Compatibility command for windows that have not yet switched to the
/// durable outcome contract. An uncertain delivery returns successfully so
/// the old composer cannot encourage a duplicate resend.
#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    draft: ComposeMessage,
) -> Result<String, String> {
    let outcome = submit_outgoing_message(Some(&app), state.inner(), &draft).await?;
    Ok(outcome.response.unwrap_or(outcome.status))
}

#[tauri::command]
async fn apply_mailbox_action(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    action: MailboxAction,
) -> Result<(), String> {
    let initial = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message is no longer available in this mailbox".to_owned())?;
    let account_id = Uuid::parse_str(&initial.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    enabled_account_for_operation(state.inner(), account_id).await?;
    let message = state
        .store
        .message(&message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message is no longer available in this mailbox".to_owned())?;
    if message.account_id != initial.account_id {
        return Err("Message changed before the action could be queued".to_owned());
    }
    let _journal = operations::enqueue_mailbox_action(&state.store, &message, action, None)
        .await
        .map_err(error)?;
    drop(_operation);
    schedule_message_mutation_drain(app, state.inner().clone(), account_id);
    Ok(())
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
    let _account = enabled_account_for_operation(state.inner(), account_id).await?;
    let service = MailService::new(state.store.clone());
    let outcome = match service.unsubscribe(&message).await {
        Ok(outcome) => outcome,
        // Older catalogue rows may contain an action selected by a previous
        // parser version. Refresh only malformed, side-effect-free web/mailto
        // metadata so a later valid fallback in the header can be selected.
        // Never retry a one-click POST: its failure may be ambiguous.
        Err(_) if message.unsubscribe_kind.as_deref() != Some("one_click") => {
            let identity = state
                .store
                .capture_message_remote_identity(&message_id)
                .await
                .map_err(error)?
                .ok_or_else(|| "Message changed while it was being refreshed".to_owned())?;
            let refreshed = fetch_remote_message(state.inner(), &identity).await?;
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
            drop(_operation);
            let submission = submit_outgoing_message(Some(&app), state.inner(), &draft).await?;
            if submission.status == "uncertain" {
                return Err("The unsubscribe email may have been delivered, but Dakia could not confirm it. It will not be sent again automatically.".to_owned());
            }
            Ok(UnsubscribeResult::Completed { cleanup_target })
        }
    }
}

#[tauri::command]
async fn trash_messages_from_sender(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    sender_address: String,
) -> Result<SenderTrashResult, String> {
    let sender_address = normalize_sender_address(&sender_address)
        .ok_or_else(|| "Sender email address is invalid".to_owned())?;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    // Discovery is read-only and receipt-fenced in core. It materializes
    // matching provider rows that have not yet reached the progressive local
    // catalogue, then returns stable local message IDs for durable action
    // enqueueing. Do not hold the lifecycle lock across this bounded provider
    // scan: update or removal can invalidate its receipts instead.
    let candidates = dakia_core::connection_budget::imap_work(
        dakia_core::connection_budget::ImapPriority::History,
        MailService::new(state.store.clone())
            .discover_messages_from_sender(&account, &sender_address),
    )
    .await
    .map_err(error)?;
    let matched = candidates.len();
    let _operation = state.account_operations.acquire(account_id).await;
    // Re-check after the network scan. Every candidate still goes through the
    // atomic identity capture below, which rejects a replaced mailbox or a
    // removed account without touching a recycled UID.
    enabled_account_for_operation(state.inner(), account_id).await?;
    let mut moved = 0;
    let mut failed = 0;
    for candidate in candidates {
        let message = candidate.message;
        match operations::enqueue_mailbox_action_for_identity(
            &state.store,
            &candidate.remote_identity,
            MailboxAction::Trash,
            None,
        )
        .await
        {
            Ok(_) => moved += 1,
            Err(queue_error) => {
                // A replacement generation can invalidate an individual
                // locator while a sender cleanup is being queued. Other
                // messages remain independently durable and optimistic.
                tracing::warn!(account_id = %account_id, message_id = %message.id, error = %queue_error, "could not queue sender cleanup mailbox action");
                failed += 1;
            }
        }
    }
    drop(_operation);
    if moved > 0 {
        schedule_message_mutation_drain(app, state.inner().clone(), account_id);
    }
    Ok(SenderTrashResult {
        matched,
        moved,
        failed,
    })
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
            let acceptance_data_dir = acceptance_data_dir()?;
            if release_smoke_test && acceptance_data_dir.is_some() {
                return Err(anyhow::anyhow!(
                    "DAKIA_ACCEPTANCE_DATA_DIR cannot be used with release smoke tests"
                )
                .into());
            }
            let data_dir = if let Some(data_dir) = acceptance_data_dir {
                data_dir
            } else if release_smoke_test {
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
                store.migrate_mail_rebuild_jobs_to_sync_runs().await?;
                store.recover_orphan_account_operation_gates().await?;
                for account in store.accounts().await? {
                    // A process can die after SMTP DATA but before a final
                    // response or journal transition. Fence every retained
                    // submission at startup before any background worker can
                    // inspect pending operations.
                    operations::recover_interrupted_account_operations(&store, account.id).await?;
                }
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
                    progressive_sync_cancellations: ProgressiveSyncCancellations::default(),
                    sync_runs_running: Mutex::new(HashSet::new()),
                    mutation_drains_running: Mutex::new(HashSet::new()),
                    folder_promotions_running: Mutex::new(HashSet::new()),
                    sent_reconcile_retry_running: Mutex::new(HashSet::new()),
                    sent_reconcile_poll_running: Mutex::new(HashSet::new()),
                    account_operations: AccountOperationLocks::default(),
                    submissions: Arc::new(SubmissionCoordinator::default()),
                    remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
                    translation_downloads: Mutex::new(HashMap::new()),
                }))
            })?;
            app.manage(state.clone());
            if cfg!(debug_assertions)
                && std::env::var("DAKIA_ACCEPTANCE_METRICS").as_deref() == Ok("1")
            {
                app.listen("dakia:mail-publication-metric", |event| {
                    println!("DAKIA_METRIC {}", event.payload());
                });
            }
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
                match state.store.accounts().await {
                    Ok(accounts) => {
                        for account in accounts {
                            if account.enabled {
                                reconcile_pending_sent_copies(
                                    Some(realtime_app.clone()),
                                    state.clone(),
                                    account.id,
                                );
                                schedule_sent_reconciliation_poll(
                                    realtime_app.clone(),
                                    state.clone(),
                                    account.id,
                                );
                                schedule_message_mutation_drain(
                                    realtime_app.clone(),
                                    state.clone(),
                                    account.id,
                                );
                                match state.store.sync_run(account.id).await {
                                    Ok(Some(run)) if run.inbox_ready => {
                                        // Header visibility and historical
                                        // backfill are independent durable
                                        // milestones. Start realtime now, but
                                        // also restore any failed/running
                                        // history run at its persisted due
                                        // time.
                                        state
                                            .realtime
                                            .start_account(realtime_app.clone(), account.clone())
                                            .await;
                                        resume_durable_sync_run(
                                            realtime_app.clone(),
                                            state.clone(),
                                            account,
                                        )
                                        .await;
                                    }
                                    Ok(_) => {
                                        // The first short EXAMINE/FETCH owns
                                        // the initial UID namespace before
                                        // realtime begins writing it.
                                        resume_durable_sync_run(
                                            realtime_app.clone(),
                                            state.clone(),
                                            account,
                                        )
                                        .await;
                                    }
                                    Err(error) => tracing::warn!(
                                        account_id = %account.id,
                                        error = %error,
                                        "could not load native sync status"
                                    ),
                                }
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
            update_account,
            show_account_context_menu,
            show_email_address_context_menu,
            remove_account,
            open_external_url,
            add_account,
            search,
            search_smart_inbox,
            conversation_for_target,
            search_remote,
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
            mail_sync_status,
            mail_unresolved_operations,
            sync_account,
            send_message,
            send_message_outcome,
            outgoing_operation_draft,
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
