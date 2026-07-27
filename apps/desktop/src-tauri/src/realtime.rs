use dakia_core::{Account, MailService, MailSummary, RealtimeMode, Store};
use serde::Serialize;
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, watch, Mutex, RwLock, Semaphore};
use uuid::Uuid;

const REALTIME_BATCH_LIMIT: u32 = 300;
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const HYDRATION_CONCURRENCY: usize = 3;
const HYDRATION_QUEUE_CAPACITY: usize = 2;

struct WatcherTask {
    cancel: watch::Sender<bool>,
    handle: tauri::async_runtime::JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeSyncStatus {
    pub account_id: Uuid,
    pub state: String,
    pub retry_at: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailArrival {
    event_id: Uuid,
    account_id: Uuid,
    messages: Vec<MailSummary>,
    detected_at: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailHydrated {
    account_id: Uuid,
    message_id: String,
}

#[derive(Clone)]
pub struct RealtimeSyncManager {
    store: Store,
    tasks: Arc<Mutex<HashMap<Uuid, WatcherTask>>>,
    statuses: Arc<RwLock<HashMap<Uuid, RealtimeSyncStatus>>>,
    reconcile_lock: Arc<Mutex<()>>,
    hydration_slots: Arc<Semaphore>,
}

impl RealtimeSyncManager {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            statuses: Arc::new(RwLock::new(HashMap::new())),
            reconcile_lock: Arc::new(Mutex::new(())),
            hydration_slots: Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
        }
    }

    pub async fn reconcile(&self, app: AppHandle) -> anyhow::Result<()> {
        // Native startup and a mounted webview can request reconciliation at
        // nearly the same time. Serialize the replacement so the later call
        // cannot cancel half of the task set created by the earlier one.
        let _reconcile = self.reconcile_lock.lock().await;
        self.stop_all().await;
        for account in self.store.accounts().await? {
            if account.enabled {
                self.start_account(app.clone(), account).await;
            }
        }
        Ok(())
    }

    pub async fn start_account(&self, app: AppHandle, account: Account) {
        self.stop_account(account.id).await;
        let (cancel, receiver) = watch::channel(false);
        let store = self.store.clone();
        let statuses = self.statuses.clone();
        let hydration_slots = self.hydration_slots.clone();
        let account_id = account.id;
        let handle = tauri::async_runtime::spawn(async move {
            run_watcher(app, store, statuses, account, receiver, hydration_slots).await;
        });
        self.tasks
            .lock()
            .await
            .insert(account_id, WatcherTask { cancel, handle });
    }

    pub async fn stop_account(&self, account_id: Uuid) {
        let task = self.tasks.lock().await.remove(&account_id);
        if let Some(task) = task {
            let _ = task.cancel.send(true);
            let _ = task.handle.await;
        }
        self.statuses.write().await.remove(&account_id);
    }

    pub async fn stop_all(&self) {
        let tasks = self
            .tasks
            .lock()
            .await
            .drain()
            .map(|(_, task)| task)
            .collect::<Vec<_>>();
        for task in &tasks {
            let _ = task.cancel.send(true);
        }
        for task in tasks {
            let _ = task.handle.await;
        }
        self.statuses.write().await.clear();
    }

    pub async fn statuses(&self) -> Vec<RealtimeSyncStatus> {
        let mut statuses = self
            .statuses
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        statuses.sort_by_key(|status| status.account_id);
        statuses
    }
}

async fn run_watcher(
    app: AppHandle,
    store: Store,
    statuses: Arc<RwLock<HashMap<Uuid, RealtimeSyncStatus>>>,
    account: Account,
    mut cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
) {
    let mut failures = 0_usize;
    // A single owned worker serializes cycle batches for this account. The
    // bounded channel provides backpressure instead of dropping later arrivals
    // or accumulating detached tasks while an earlier batch is still active.
    let (hydration_sender, hydration_receiver) =
        mpsc::channel::<Vec<MailSummary>>(HYDRATION_QUEUE_CAPACITY);
    let (hydration_cancel, hydration_cancel_receiver) = watch::channel(false);
    let hydration_handle = tauri::async_runtime::spawn(run_hydration_worker(
        app.clone(),
        store.clone(),
        account.clone(),
        hydration_receiver,
        hydration_cancel_receiver,
        hydration_slots,
    ));
    loop {
        if *cancel.borrow() {
            break;
        }
        publish_status(&app, &statuses, account.id, "connecting", None, None).await;
        let service = MailService::new(store.clone());
        match service
            .realtime_inbox_cycle(&account, REALTIME_BATCH_LIMIT, &mut cancel)
            .await
        {
            Ok(cycle) if cycle.cancelled => break,
            Ok(cycle) => {
                failures = 0;
                let state = if cycle.mode == RealtimeMode::Idle {
                    "idle"
                } else {
                    "polling"
                };
                publish_status(&app, &statuses, account.id, state, None, None).await;
                if !cycle.new_messages.is_empty() {
                    let detected_at = cycle.detected_at.unwrap_or_else(chrono::Utc::now);
                    tracing::info!(
                        account_id = %account.id,
                        provider = %account.provider_id,
                        message_count = cycle.new_messages.len(),
                        "new mail headers persisted"
                    );
                    let _ = app.emit(
                        "mail-arrived",
                        MailArrival {
                            event_id: Uuid::new_v4(),
                            account_id: account.id,
                            messages: cycle.new_messages.clone(),
                            detected_at: detected_at.to_rfc3339(),
                        },
                    );
                }
                // Queue every cycle, including an empty pending list, because
                // the worker also revisits uncached starred messages. If the
                // small queue is full, wait with cancellation rather than lose
                // a cycle's only copy of its new-message hydration candidates.
                let pending_hydration = cycle.pending_hydration;
                tokio::select! {
                    sent = hydration_sender.send(pending_hydration) => {
                        if sent.is_err() {
                            tracing::warn!(
                                account_id = %account.id,
                                "background hydration worker stopped unexpectedly"
                            );
                            break;
                        }
                    }
                    _ = wait_for_cancellation(&mut cancel) => break,
                }
                if cycle.mode == RealtimeMode::Poll
                    && wait_or_cancel(poll_delay(account.id), &mut cancel).await
                {
                    break;
                }
            }
            Err(error) => {
                let message = error.to_string();
                if message.to_ascii_lowercase().contains("authentication") {
                    tracing::warn!(account_id = %account.id, provider = %account.provider_id, "real-time sync authentication paused");
                    publish_status(
                        &app,
                        &statuses,
                        account.id,
                        "paused",
                        None,
                        Some("authentication"),
                    )
                    .await;
                    break;
                }
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures, account.id);
                let retry_at = (chrono::Utc::now()
                    + chrono::Duration::from_std(delay).unwrap_or_default())
                .to_rfc3339();
                tracing::warn!(
                    account_id = %account.id,
                    provider = %account.provider_id,
                    retry_seconds = delay.as_secs(),
                    error = %error,
                    "real-time sync reconnect scheduled"
                );
                publish_status(
                    &app,
                    &statuses,
                    account.id,
                    "retrying",
                    Some(retry_at),
                    Some("connection"),
                )
                .await;
                if wait_or_cancel(delay, &mut cancel).await {
                    break;
                }
            }
        }
    }
    let _ = hydration_cancel.send(true);
    drop(hydration_sender);
    let _ = hydration_handle.await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HydrationKind {
    Pending,
    Starred,
}

#[derive(Clone)]
struct HydrationTarget {
    kind: HydrationKind,
    message: MailSummary,
}

struct HydrationResult {
    target: HydrationTarget,
    started: std::time::Instant,
    result: anyhow::Result<Option<MailSummary>>,
    cancelled: bool,
}

fn hydration_plan(pending: Vec<MailSummary>, starred: Vec<MailSummary>) -> Vec<HydrationTarget> {
    let mut seen = std::collections::HashSet::new();
    pending
        .into_iter()
        .map(|message| HydrationTarget {
            kind: HydrationKind::Pending,
            message,
        })
        .chain(starred.into_iter().map(|message| HydrationTarget {
            kind: HydrationKind::Starred,
            message,
        }))
        .filter(|target| seen.insert(target.message.id.clone()))
        .collect()
}

async fn run_hydration_worker(
    app: AppHandle,
    store: Store,
    account: Account,
    receiver: mpsc::Receiver<Vec<MailSummary>>,
    cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
) {
    let service = MailService::new(store.clone());
    run_queued_hydration_batches(receiver, cancel, move |pending, cancel| {
        let app = app.clone();
        let store = store.clone();
        let service = service.clone();
        let account = account.clone();
        let hydration_slots = hydration_slots.clone();
        async move {
            hydrate_after_sync(
                app,
                store,
                service,
                account,
                pending,
                cancel,
                hydration_slots,
            )
            .await;
        }
    })
    .await;
}

async fn run_queued_hydration_batches<T, F, Fut>(
    mut receiver: mpsc::Receiver<T>,
    mut cancel: watch::Receiver<bool>,
    mut operation: F,
) where
    T: Send + 'static,
    F: FnMut(T, watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let item = tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel) => break,
            item = receiver.recv() => item,
        };
        let Some(item) = item else {
            break;
        };
        operation(item, cancel.clone()).await;
    }
}

async fn hydrate_after_sync(
    app: AppHandle,
    store: Store,
    service: MailService,
    account: Account,
    pending: Vec<MailSummary>,
    cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
) {
    let starred = match store.uncached_starred_messages(account.id, 25).await {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(
                account_id = %account.id,
                error = %error,
                "could not load starred messages for background hydration"
            );
            Vec::new()
        }
    };
    let plan = hydration_plan(pending, starred);
    let hydration_account = account.clone();
    let results = run_bounded_cancellable_ordered(
        plan,
        HYDRATION_CONCURRENCY,
        hydration_slots,
        cancel,
        move |target, cancel| {
            let store = store.clone();
            let service = service.clone();
            let account = hydration_account.clone();
            async move { hydrate_target(store, service, account, target, cancel).await }
        },
    )
    .await;

    // The bounded runner restores plan order before user-visible events and
    // logs, even though IMAP requests may finish in a different order.
    for outcome in results {
        if outcome.cancelled {
            continue;
        }
        match outcome.result {
            Ok(Some(hydrated)) if outcome.target.kind == HydrationKind::Pending => {
                tracing::info!(
                    account_id = %account.id,
                    message_id = %hydrated.id,
                    hydration_ms = outcome.started.elapsed().as_millis(),
                    "new mail hydration complete"
                );
                let _ = app.emit(
                    "mail-hydrated",
                    MailHydrated {
                        account_id: account.id,
                        message_id: hydrated.id,
                    },
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    account_id = %account.id,
                    message_id = %outcome.target.message.id,
                    error = %error,
                    "background mail hydration failed"
                );
            }
        }
    }
}

async fn hydrate_target(
    store: Store,
    service: MailService,
    account: Account,
    target: HydrationTarget,
    mut cancel: watch::Receiver<bool>,
) -> HydrationResult {
    let started = std::time::Instant::now();
    if *cancel.borrow() {
        return HydrationResult {
            target,
            started,
            result: Ok(None),
            cancelled: true,
        };
    }

    if target.kind == HydrationKind::Pending {
        match store.claim_message_hydration(&target.message.id).await {
            Ok(true) => {}
            Ok(false) => {
                return HydrationResult {
                    target,
                    started,
                    result: Ok(None),
                    cancelled: false,
                };
            }
            Err(error) => {
                return HydrationResult {
                    target,
                    started,
                    result: Err(error),
                    cancelled: false,
                };
            }
        }
    }

    let (result, cancelled) = {
        let hydration = async {
            match target.kind {
                HydrationKind::Pending => {
                    service
                        .hydrate_inbox_message(&account, target.message.uid as u32)
                        .await
                }
                HydrationKind::Starred => {
                    service
                        .hydrate_message(
                            &account,
                            &target.message.mailbox,
                            target.message.uid as u32,
                        )
                        .await
                }
            }
        };
        tokio::pin!(hydration);
        tokio::select! {
            result = &mut hydration => (result.map(Some), false),
            _ = wait_for_cancellation(&mut cancel) => (Ok(None), true),
        }
    };
    if target.kind == HydrationKind::Pending && (cancelled || result.is_err()) {
        let _ = store
            .set_message_content_state(&target.message.id, "failed")
            .await;
    }
    HydrationResult {
        target,
        started,
        result,
        cancelled,
    }
}

async fn wait_for_cancellation(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn run_bounded_cancellable_ordered<T, U, F, Fut>(
    items: Vec<T>,
    max_in_flight: usize,
    limiter: Arc<Semaphore>,
    mut cancel: watch::Receiver<bool>,
    operation: F,
) -> Vec<U>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T, watch::Receiver<bool>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = U> + Send + 'static,
{
    assert!(max_in_flight > 0, "bounded work requires a non-zero limit");
    let mut pending = items.into_iter().enumerate();
    let mut active = tokio::task::JoinSet::new();
    let mut completed = Vec::new();
    let mut cancelled = *cancel.borrow();

    loop {
        // A worker can observe cancellation and finish before this coordinator
        // wins its own watch branch. Re-read the current value before filling
        // the next slot so no post-cancellation item can be scheduled.
        cancelled |= *cancel.borrow();
        while !cancelled && active.len() < max_in_flight {
            let Some((index, item)) = pending.next() else {
                break;
            };
            let operation = operation.clone();
            let mut task_cancel = cancel.clone();
            let limiter = limiter.clone();
            active.spawn(async move {
                let permit = tokio::select! {
                    permit = limiter.acquire_owned() => permit.ok(),
                    _ = wait_for_cancellation(&mut task_cancel) => None,
                };
                let Some(permit) = permit else {
                    return (index, None);
                };
                let result = operation(item, task_cancel).await;
                drop(permit);
                (index, Some(result))
            });
        }
        if active.is_empty() {
            break;
        }
        tokio::select! {
            changed = cancel.changed(), if !cancelled => {
                cancelled = changed.is_err() || *cancel.borrow();
            }
            joined = active.join_next() => {
                if let Some(joined) = joined {
                    completed.push(joined.expect("bounded hydration task must not panic"));
                }
            }
        }
    }

    completed.sort_by_key(|(index, _)| *index);
    completed
        .into_iter()
        .filter_map(|(_, result)| result)
        .collect()
}

async fn publish_status(
    app: &AppHandle,
    statuses: &RwLock<HashMap<Uuid, RealtimeSyncStatus>>,
    account_id: Uuid,
    state: &str,
    retry_at: Option<String>,
    error_kind: Option<&str>,
) {
    let status = RealtimeSyncStatus {
        account_id,
        state: state.to_owned(),
        retry_at,
        error_kind: error_kind.map(str::to_owned),
    };
    statuses.write().await.insert(account_id, status.clone());
    let _ = app.emit("mail-sync-state", status);
}

async fn wait_or_cancel(delay: Duration, cancel: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        changed = cancel.changed() => changed.is_err() || *cancel.borrow(),
    }
}

fn retry_delay(failures: usize, account_id: Uuid) -> Duration {
    let seconds = [1_u64, 2, 5, 10, 30, 60][failures.saturating_sub(1).min(5)];
    jitter(Duration::from_secs(seconds), account_id)
}

fn poll_delay(account_id: Uuid) -> Duration {
    // IDLE remains the preferred path, but some providers do not advertise it
    // (and some proxies strip it). Poll conservatively so a permanently open
    // client does not create avoidable load on the provider. Account-specific
    // jitter also prevents every configured account from polling in lockstep.
    jitter(FALLBACK_POLL_INTERVAL, account_id)
}

fn jitter(base: Duration, account_id: Uuid) -> Duration {
    let offset = i64::from(account_id.as_bytes()[0] % 21) - 10;
    let millis = base.as_millis() as i64;
    Duration::from_millis((millis + millis * offset / 100).max(100) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn message(id: &str) -> MailSummary {
        MailSummary {
            id: id.into(),
            account_id: Uuid::nil().to_string(),
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: id.into(),
            subject: id.into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
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
            attachments: Vec::new(),
        }
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered() {
        let id = Uuid::from_bytes([7; 16]);
        assert!(retry_delay(1, id) < Duration::from_secs(2));
        assert!(retry_delay(100, id) <= Duration::from_secs(66));
    }

    #[test]
    fn fallback_polling_is_five_minutes_with_account_jitter() {
        let delay = poll_delay(Uuid::from_bytes([20; 16]));
        assert!((Duration::from_secs(270)..=Duration::from_secs(330)).contains(&delay));
    }

    #[test]
    fn hydration_plan_prioritizes_pending_and_deduplicates_starred() {
        let plan = hydration_plan(
            vec![message("pending-1"), message("both")],
            vec![message("both"), message("starred-1")],
        );
        assert_eq!(
            plan.iter()
                .map(|target| (target.message.id.as_str(), target.kind))
                .collect::<Vec<_>>(),
            vec![
                ("pending-1", HydrationKind::Pending),
                ("both", HydrationKind::Pending),
                ("starred-1", HydrationKind::Starred),
            ]
        );
    }

    #[tokio::test]
    async fn hydration_worker_queues_a_second_cycle_while_the_first_is_active() {
        let (sender, receiver) = mpsc::channel(1);
        let (_cancel, cancel_receiver) = watch::channel(false);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let processed = Arc::new(Mutex::new(Vec::new()));
        let worker = tokio::spawn(run_queued_hydration_batches(receiver, cancel_receiver, {
            let started = started.clone();
            let release = release.clone();
            let processed = processed.clone();
            move |batch: Vec<u32>, _| {
                let started = started.clone();
                let release = release.clone();
                let processed = processed.clone();
                async move {
                    if batch == [1] {
                        started.notify_one();
                        release.notified().await;
                    }
                    processed.lock().await.extend(batch);
                }
            }
        }));

        sender.send(vec![1]).await.unwrap();
        started.notified().await;
        sender
            .send(vec![2])
            .await
            .expect("the later realtime cycle must be queued");
        drop(sender);
        release.notify_one();
        worker.await.unwrap();

        assert_eq!(*processed.lock().await, vec![1, 2]);
    }

    #[tokio::test]
    async fn hydration_runner_caps_concurrency_and_restores_plan_order() {
        let (_cancel, receiver) = watch::channel(false);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let results = run_bounded_cancellable_ordered(
            (0..10).collect(),
            3,
            Arc::new(Semaphore::new(3)),
            receiver,
            {
                let active = active.clone();
                let peak = peak.clone();
                move |index, _| {
                    let active = active.clone();
                    let peak = peak.clone();
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis((10 - index) as u64)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        index
                    }
                }
            },
        )
        .await;

        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(results, (0..10).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn hydration_runner_stops_scheduling_after_cancellation() {
        let (cancel, receiver) = watch::channel(false);
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_bounded_cancellable_ordered(
            (0..20).collect(),
            2,
            Arc::new(Semaphore::new(2)),
            receiver,
            {
                let started = started.clone();
                let completed = completed.clone();
                move |index, mut cancel| {
                    let started = started.clone();
                    let completed = completed.clone();
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        wait_for_cancellation(&mut cancel).await;
                        completed.fetch_add(1, Ordering::SeqCst);
                        index
                    }
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the initial bounded batch should start");
        assert_eq!(started.load(Ordering::SeqCst), 2);

        cancel.send(true).unwrap();
        let results = task.await.unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert_eq!(results, vec![0, 1]);
    }

    #[tokio::test]
    async fn hydration_accounts_share_the_manager_wide_limit() {
        let limiter = Arc::new(Semaphore::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let invoke = |offset| {
            let (cancel, receiver) = watch::channel(false);
            let limiter = limiter.clone();
            let active = active.clone();
            let peak = peak.clone();
            tokio::spawn(async move {
                let _cancel = cancel;
                run_bounded_cancellable_ordered(
                    (0..8).map(|index| offset + index).collect(),
                    3,
                    limiter,
                    receiver,
                    move |index, _| {
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
                )
                .await
            })
        };

        let (left, right) = tokio::join!(invoke(0), invoke(100));
        assert_eq!(left.unwrap().len(), 8);
        assert_eq!(right.unwrap().len(), 8);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(limiter.available_permits(), 3);
    }

    #[tokio::test]
    async fn cancellation_releases_tasks_waiting_for_a_global_slot() {
        let limiter = Arc::new(Semaphore::new(1));
        let held = limiter.clone().acquire_owned().await.unwrap();
        let (cancel, receiver) = watch::channel(false);
        let started = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_bounded_cancellable_ordered(
            vec![1],
            1,
            limiter.clone(),
            receiver,
            {
                let started = started.clone();
                move |value, _| {
                    let started = started.clone();
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        value
                    }
                }
            },
        ));
        tokio::task::yield_now().await;
        cancel.send(true).unwrap();

        let results = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("account stop must cancel global-slot waiters")
            .unwrap();
        assert!(results.is_empty());
        assert_eq!(started.load(Ordering::SeqCst), 0);
        drop(held);
        assert_eq!(limiter.available_permits(), 1);
    }
}
