mod realtime;
mod translation;

#[cfg(test)]
mod tauri_contracts_tests;

use base64::{engine::general_purpose::STANDARD, Engine};
use dakia_core::storage::{ConversationTarget, MessageContentFetchAcquire};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, provider, Account, AccountAuth, AccountDraft, Attachment,
    CachedMessageContent, ComposeMessage, LocalEmailClassifier, MailConversation,
    MailConversationPage, MailRebuildJob, MailService, MailSummary, MailboxAction, OAuthFlow,
    OAuthProviderConfig, ProviderPreset, SearchQuery, Store, SyncProgress, SyncResult,
    UnsubscribeOutcome,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use std::{
    collections::HashMap,
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
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, Semaphore};
use url::Url;
use uuid::Uuid;

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

struct AppState {
    store: Store,
    data_dir: PathBuf,
    classifier: Mutex<Box<dyn EmailClassifier>>,
    classification: Arc<ClassificationScheduler>,
    realtime: RealtimeSyncManager,
    remote_operation_slots: Arc<Semaphore>,
    mail_rebuilds: Mutex<HashMap<Uuid, MailRebuildProgress>>,
    account_operations: AccountOperationLocks,
    translation_downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

trait EmailClassifier: Send {
    fn classify(
        &mut self,
        emails: &[String],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>>;
}

impl EmailClassifier for LocalEmailClassifier {
    fn classify(
        &mut self,
        emails: &[String],
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

fn normalized_account_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn matching_account_email(account: &Account, email: &str) -> bool {
    normalized_account_email(&account.email) == normalized_account_email(email)
}

fn credential_secret_name(account: &Account) -> String {
    // Keep this aligned with `dakia_core::mail::CredentialStore::key` so an
    // OAuth save failure can restore a credential it just replaced.
    format!("dev.dakia.mail:{}:{}", account.id, account.auth.username())
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

    #[test]
    fn normalizes_email_identity_for_account_reuse() {
        assert_eq!(
            normalized_account_email(" Existing@Example.Com "),
            "existing@example.com"
        );
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
}

impl From<MailRebuildJob> for MailRebuildProgress {
    fn from(job: MailRebuildJob) -> Self {
        Self {
            account_id: job.account_id,
            phase: job.phase,
            completed: job.completed,
            total: job.total,
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
    Completed,
    OpenedWeb,
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
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
            _emails: &[String],
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
            classification: Arc::new(ClassificationScheduler::default()),
            mail_rebuilds: Mutex::new(HashMap::new()),
            account_operations: AccountOperationLocks::default(),
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
    let _operation = state.account_operations.acquire(input.id).await;
    let mut account = state
        .store
        .account(input.id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
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
    if let Some(password) = input.password.filter(|value| !value.is_empty()) {
        if !matches!(account.auth, AccountAuth::Password { .. }) {
            return Err("OAuth accounts must be reconnected through their provider".into());
        }
        MailService::new(state.store.clone())
            .credentials()
            .set_password(&account, &password)
            .await
            .map_err(error)?;
    }
    state.store.save_account(&account).await.map_err(error)?;
    state.realtime.reconcile(app).await.map_err(error)?;
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

#[tauri::command]
async fn remove_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
) -> Result<(), String> {
    let _operation = state.account_operations.acquire(account_id).await;
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
) -> Result<Account, String> {
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
    let mut account = input.draft.into_account(preset);
    let stored_accounts = state.store.accounts().await.map_err(error)?;
    let mut reused_existing_account = false;
    if let Some(existing) = stored_accounts
        .into_iter()
        .find(|stored| matching_account_email(stored, &account.email))
    {
        account.id = existing.id;
        account.created_at = existing.created_at;
        account.account_name = existing.account_name;
        reused_existing_account = true;
    }
    let _operation = state.account_operations.acquire(account.id).await;
    if reused_existing_account
        && state
            .store
            .account(account.id)
            .await
            .map_err(error)?
            .is_none()
    {
        return Err("Account not found".into());
    }
    let mail = MailService::new(state.store.clone());
    mail.credentials()
        .set_password(&account, &input.password)
        .await
        .map_err(error)?;
    state.store.save_account(&account).await.map_err(error)?;
    state.realtime.reconcile(app).await.map_err(error)?;
    Ok(account)
}

#[tauri::command]
async fn add_oauth_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    draft: AccountDraft,
) -> Result<Account, String> {
    let preset = draft
        .provider_id
        .as_deref()
        .and_then(provider::by_id)
        .unwrap_or_else(|| provider::detect(&draft.email));
    if !preset.oauth {
        return Err("The selected provider does not offer OAuth sign-in".into());
    }
    let client_id = oauth_client_id(preset.id)?;
    let username = draft
        .username
        .clone()
        .unwrap_or_else(|| draft.email.clone());
    let email = draft.email.clone();
    let client_secret = oauth_client_secret(preset.id)?;
    let config = OAuthProviderConfig::for_provider(preset.id, client_id)
        .map_err(error)?
        .with_client_secret(client_secret);
    let (flow, authorization_url) = OAuthFlow::start(config, Some(&email))
        .await
        .map_err(error)?;
    app.opener()
        .open_url(authorization_url.as_str(), None::<&str>)
        .map_err(error)?;
    let tokens = flow.finish().await.map_err(error)?;
    let mut account = draft.into_account(preset);
    account.auth = AccountAuth::OAuth2 {
        username,
        provider: preset.id.into(),
        access_token_expires_at: tokens.expires_at,
    };
    let stored_accounts = state.store.accounts().await.map_err(error)?;
    let mut reused_existing_account = false;
    if let Some(existing) = stored_accounts
        .into_iter()
        .find(|stored| matching_account_email(stored, &account.email))
    {
        account.id = existing.id;
        account.created_at = existing.created_at;
        account.account_name = existing.account_name;
        reused_existing_account = true;
    }
    let _operation = state.account_operations.acquire(account.id).await;
    let existing_account = if reused_existing_account {
        Some(
            state
                .store
                .account(account.id)
                .await
                .map_err(error)?
                .ok_or_else(|| "Account not found".to_owned())?,
        )
    } else {
        None
    };
    let mail = MailService::new(state.store.clone());
    let oauth_secret_name = credential_secret_name(&account);
    let replaced_credential = if let Some(existing) = existing_account {
        let existing_secret_name = credential_secret_name(&existing);
        if existing_secret_name == oauth_secret_name {
            state
                .store
                .secret(&existing_secret_name)
                .await
                .map_err(error)?
        } else {
            None
        }
    } else {
        None
    };
    mail.credentials()
        .set_oauth_tokens(&account, &tokens)
        .await
        .map_err(error)?;
    if let Err(save_error) = state.store.save_account(&account).await {
        let rollback = match replaced_credential {
            Some(previous_credential) => {
                state
                    .store
                    .set_secret(&oauth_secret_name, &previous_credential)
                    .await
            }
            None => mail.credentials().delete(&account).await,
        };
        if let Err(rollback_error) = rollback {
            tracing::error!(
                account_id = %account.id,
                error = %rollback_error,
                "could not roll back OAuth credentials after saving the account failed"
            );
        }
        return Err(error(save_error));
    }
    state.realtime.reconcile(app.clone()).await.map_err(error)?;
    Ok(account)
}

const GOOGLE_DESKTOP_CLIENT_ID: &str =
    "77400090557-np3jvrl1d13oec7i9evs0i9c89u7q3hg.apps.googleusercontent.com";

fn oauth_client_id(provider: &str) -> Result<String, String> {
    let (key, compiled, default) = match provider {
        "gmail" => (
            "DAKIA_GOOGLE_CLIENT_ID",
            option_env!("DAKIA_GOOGLE_CLIENT_ID"),
            Some(GOOGLE_DESKTOP_CLIENT_ID),
        ),
        "outlook" => (
            "DAKIA_MICROSOFT_CLIENT_ID",
            option_env!("DAKIA_MICROSOFT_CLIENT_ID"),
            None,
        ),
        "yahoo" => (
            "DAKIA_YAHOO_CLIENT_ID",
            option_env!("DAKIA_YAHOO_CLIENT_ID"),
            None,
        ),
        _ => {
            return Err(format!(
                "OAuth client registration is unavailable for {provider}"
            ))
        }
    };
    resolve_oauth_client_id(std::env::var(key).ok(), compiled, default, key)
}

fn resolve_oauth_client_id(
    runtime: Option<String>,
    compiled: Option<&str>,
    default: Option<&str>,
    key: &str,
) -> Result<String, String> {
    runtime
        .filter(|value| !value.is_empty())
        .or_else(|| {
            compiled
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| default.filter(|value| !value.is_empty()).map(str::to_owned))
        .ok_or_else(|| format!("This build is missing {key}"))
}

fn oauth_client_secret(provider: &str) -> Result<Option<String>, String> {
    if provider != "gmail" {
        return Ok(None);
    }
    let key = "DAKIA_GOOGLE_CLIENT_SECRET";
    std::env::var(key)
        .ok()
        .or_else(|| option_env!("DAKIA_GOOGLE_CLIENT_SECRET").map(str::to_owned))
        .filter(|value| !value.is_empty())
        .map(Some)
        .ok_or_else(|| format!("This build is missing {key}"))
}

#[cfg(test)]
mod oauth_client_id_tests {
    use super::*;

    #[test]
    fn gmail_uses_the_configured_desktop_client_by_default() {
        assert!(!oauth_client_id("gmail")
            .expect("Gmail OAuth client")
            .is_empty());
    }

    #[test]
    fn empty_oauth_client_overrides_fall_back_to_the_default() {
        assert_eq!(
            resolve_oauth_client_id(
                Some(String::new()),
                Some(""),
                Some(GOOGLE_DESKTOP_CLIENT_ID),
                "DAKIA_GOOGLE_CLIENT_ID",
            )
            .expect("default client ID"),
            GOOGLE_DESKTOP_CLIENT_ID
        );
    }
}

#[tauri::command]
async fn search(
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<MailConversationPage, String> {
    state
        .store
        .search_conversation_page(&query)
        .await
        .map_err(error)
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
    let messages = state
        .store
        .messages_for_model_classification_batch(CLASSIFICATION_BATCH_SIZE)
        .await?;
    if messages.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = messages.iter().map(|message| message.id.clone()).collect();
    let inputs: Vec<String> = messages
        .iter()
        .map(|message| {
            dakia_core::classification::email_text(
                message.from_name.as_deref(),
                &message.from_address,
                &message.subject,
                &message.snippet,
                &message.classification_signals,
            )
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
    let updates: Vec<(String, String, f64)> = ids
        .into_iter()
        .zip(classifications)
        .map(|(id, result)| (id, result.category, result.confidence))
        .collect();
    let count = updates.len();
    state.store.apply_model_classifications(&updates).await?;
    Ok(count)
}

async fn drain_pending_classifications(state: Arc<AppState>) -> anyhow::Result<()> {
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
    let update = MailRebuildProgress {
        account_id,
        phase: progress.phase.to_owned(),
        completed: progress.completed,
        total: progress.total,
    };
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account_id, update.clone());
    let _ = app.emit("mail-rebuild-progress", &update);
}

async fn run_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
) -> anyhow::Result<SyncResult> {
    let _operation = state.account_operations.acquire(account.id).await;
    let account = state
        .store
        .account(account.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Account not found"))?;
    run_mail_rebuild_locked(app, state, account, reset_before_sync).await
}

async fn run_mail_rebuild_locked(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
) -> anyhow::Result<SyncResult> {
    state.realtime.stop_account(account.id).await;
    let initial = MailRebuildJob {
        account_id: account.id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
    };
    state.store.save_mail_rebuild_job(&initial).await?;
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account.id, initial.into());

    let service = MailService::new(state.store.clone());
    let progress_app = app.clone();
    let progress_state = state.clone();
    let account_id = account.id;
    let result = if reset_before_sync {
        service
            .rebuild_all_with_progress(&account, 250, move |progress| {
                publish_mail_rebuild_progress(&progress_app, &progress_state, account_id, progress);
            })
            .await
    } else {
        service
            .resume_rebuild_all_with_progress(&account, 250, move |progress| {
                publish_mail_rebuild_progress(&progress_app, &progress_state, account_id, progress);
            })
            .await
    };

    if result.is_ok() {
        state.store.delete_mail_rebuild_job(account.id).await?;
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
        let _ = app.emit(
            "mail-index-rebuilt",
            serde_json::json!({ "accountId": account.id }),
        );
        kick_classification(state.clone());
    } else {
        if let Err(error) = state.store.delete_mail_rebuild_job(account.id).await {
            tracing::warn!(
                account_id = %account.id,
                error = %error,
                "could not clear failed mail rebuild job"
            );
        }
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
    }
    restart_realtime_if_current(app, &state, account.id).await?;
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
        let account = state
            .store
            .account(account_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Account not found".to_owned())?;
        if state
            .mail_rebuilds
            .lock()
            .map_err(error)?
            .contains_key(&account_id)
        {
            return Err("A mail re-index is already running for this account".to_owned());
        }
        let result =
            run_mail_rebuild(app.clone(), state.inner().clone(), account.clone(), true).await;
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

#[tauri::command]
async fn send_message(
    state: State<'_, Arc<AppState>>,
    draft: ComposeMessage,
) -> Result<String, String> {
    let _operation = state.account_operations.acquire(draft.account_id).await;
    let account = enabled_account_for_operation(state.inner(), draft.account_id).await?;
    MailService::new(state.store.clone())
        .send(&account, &draft)
        .await
        .map_err(error)
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
            service.unsubscribe(&refreshed).await.map_err(error)?
        }
        Err(failure) => return Err(error(failure)),
    };
    match outcome {
        UnsubscribeOutcome::Completed => Ok(UnsubscribeResult::Completed),
        UnsubscribeOutcome::Web(url) => {
            open_external_url(app, url)?;
            Ok(UnsubscribeResult::OpenedWeb)
        }
        UnsubscribeOutcome::Mailto { to, subject, body } => {
            let draft = unsubscribe_email(account_id, to, subject, body)?;
            MailService::new(state.store.clone())
                .send(&account, &draft)
                .await
                .map_err(error)?;
            Ok(UnsubscribeResult::Completed)
        }
    }
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_menu_event(|app, event| {
            let action = event.id().as_ref();
            let message_action = matches!(action, "reply" | "forward" | "archive" | "spam");
            let focused = message_action.then(|| {
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
                    classification: Arc::new(ClassificationScheduler::default()),
                    mail_rebuilds: Mutex::new(mail_rebuilds),
                    account_operations: AccountOperationLocks::default(),
                    remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
                    translation_downloads: Mutex::new(HashMap::new()),
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
                oauth_client_id("gmail")?;
                oauth_client_secret("gmail")?;
                eprintln!("DAKIA_RELEASE_GOOGLE_OAUTH_CONFIG_OK");
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
                let rebuilding: std::collections::HashSet<_> = state
                    .mail_rebuilds
                    .lock()
                    .expect("mail rebuild lock poisoned")
                    .keys()
                    .copied()
                    .collect();
                match state.store.accounts().await {
                    Ok(accounts) => {
                        for account in accounts {
                            if rebuilding.contains(&account.id) {
                                let rebuild_app = realtime_app.clone();
                                let rebuild_state = state.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(error) =
                                        run_mail_rebuild(rebuild_app, rebuild_state, account, false)
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
            update_account,
            show_account_context_menu,
            remove_account,
            open_external_url,
            add_account,
            add_oauth_account,
            search,
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
            sync_account,
            send_message,
            apply_mailbox_action,
            unsubscribe_message,
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
