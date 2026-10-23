use crate::account_locks::{AccountOperationGuard, AccountOperationLocks};
use dakia_core::{Account, MailService, MailSummary, RealtimeMode, Store};
use serde::Serialize;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::{mpsc, watch, Mutex, RwLock, Semaphore};
use uuid::Uuid;

const REALTIME_BATCH_LIMIT: u32 = 300;
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const HYDRATION_CONCURRENCY: usize = 3;
const HYDRATION_MAINTENANCE_QUEUE_CAPACITY: usize = 1;
const BODY_CACHE_WARM_INTERVAL: Duration = Duration::from_secs(15 * 60);
const BODY_CACHE_WARM_BATCH_LIMIT: u32 = 100;
const BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE: u32 = 3;
const BODY_CACHE_WARM_LOOKBACK_DAYS: i64 = 30;
const BODY_CACHE_FAILURE_COOLDOWN: Duration = Duration::from_secs(60 * 60);

type HydrationCompleteHook = Arc<dyn Fn() + Send + Sync>;
type HydrationCompleteHookStore = Arc<StdRwLock<Option<HydrationCompleteHook>>>;

/// The Tauri event boundary is deliberately small so the watcher can be
/// exercised with its production control flow without constructing a webview.
/// Payloads are serialized before crossing this boundary, just as they are for
/// Tauri's `Emitter` implementation.
#[derive(Clone, Copy)]
enum RealtimeEvent {
    Arrived,
    Changed,
    Hydrated,
    SyncState,
}

impl RealtimeEvent {
    const fn name(self) -> &'static str {
        match self {
            Self::Arrived => "mail-arrived",
            Self::Changed => "mail-changed",
            Self::Hydrated => "mail-hydrated",
            Self::SyncState => "mail-sync-state",
        }
    }
}

trait RealtimeEventSink: Send + Sync {
    fn publish(&self, event: RealtimeEvent, payload: serde_json::Value);
}

struct TauriRealtimeEventSink<R: Runtime = tauri::Wry> {
    app: AppHandle<R>,
}

impl<R: Runtime> RealtimeEventSink for TauriRealtimeEventSink<R> {
    fn publish(&self, event: RealtimeEvent, payload: serde_json::Value) {
        let result = match event {
            RealtimeEvent::Arrived => self.app.emit("mail-arrived", payload),
            RealtimeEvent::Changed => self.app.emit("mail-changed", payload),
            RealtimeEvent::Hydrated => self.app.emit("mail-hydrated", payload),
            RealtimeEvent::SyncState => self.app.emit("mail-sync-state", payload),
        };
        if let Err(error) = result {
            let event = event.name();
            tracing::warn!(event, error = %error, "could not publish real-time mail event");
        }
    }
}

fn publish_event<T: Serialize>(sink: &dyn RealtimeEventSink, event: RealtimeEvent, payload: T) {
    match serde_json::to_value(payload) {
        Ok(payload) => sink.publish(event, payload),
        Err(error) => {
            tracing::warn!(event = event.name(), error = %error, "could not serialize real-time mail event")
        }
    }
}

trait RealtimeCycleRunner: Send {
    fn next_cycle<'a>(
        &'a mut self,
        cancel: &'a mut watch::Receiver<bool>,
        idle_ready: watch::Sender<bool>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<dakia_core::RealtimeCycle>> + Send + 'a>>;
}

trait WatcherDelay: Send + Sync {
    fn wait<'a>(
        &'a self,
        delay: Duration,
        cancel: &'a mut watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

struct TokioWatcherDelay;

impl WatcherDelay for TokioWatcherDelay {
    fn wait<'a>(
        &'a self,
        delay: Duration,
        cancel: &'a mut watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(wait_or_cancel(delay, cancel))
    }
}

struct MailServiceRealtimeCycleRunner {
    store: Store,
    account: Account,
}

impl RealtimeCycleRunner for MailServiceRealtimeCycleRunner {
    fn next_cycle<'a>(
        &'a mut self,
        cancel: &'a mut watch::Receiver<bool>,
        idle_ready: watch::Sender<bool>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<dakia_core::RealtimeCycle>> + Send + 'a>> {
        Box::pin(async move {
            MailService::new(self.store.clone())
                .realtime_inbox_cycle_with_idle_ready(
                    &self.account,
                    REALTIME_BATCH_LIMIT,
                    cancel,
                    move || {
                        idle_ready.send_if_modified(|ready| {
                            if *ready {
                                false
                            } else {
                                *ready = true;
                                true
                            }
                        });
                    },
                )
                .await
        })
    }
}

struct WatcherTask {
    cancel: watch::Sender<bool>,
    handle: tauri::async_runtime::JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeSyncStatus {
    pub account_id: Uuid,
    pub state: String,
    pub retry_at: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MailArrival {
    pub(crate) event_id: Uuid,
    pub(crate) account_id: Uuid,
    pub(crate) messages: Vec<MailSummary>,
    pub(crate) detected_at: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MailHydrated {
    pub(crate) account_id: Uuid,
    pub(crate) message_id: String,
}

#[derive(Clone)]
pub struct RealtimeSyncManager {
    store: Store,
    tasks: Arc<Mutex<HashMap<Uuid, WatcherTask>>>,
    // Replacements for one account must include removing and awaiting the old
    // watcher, spawning its successor, and registering the successor. Without
    // this lock, two callers can both observe no task and the later insertion
    // can overwrite the first handle, leaving that watcher uncancellable.
    account_task_locks: Arc<AccountOperationLocks>,
    // Reconciliation and stop-all need an exclusive snapshot of the watcher
    // registry. Individual account transitions share this lock so they cannot
    // race a full replacement sweep.
    watcher_lifecycle: Arc<RwLock<()>>,
    statuses: Arc<RwLock<HashMap<Uuid, RealtimeSyncStatus>>>,
    reconcile_lock: Arc<Mutex<()>>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
}

impl RealtimeSyncManager {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            account_task_locks: Arc::new(AccountOperationLocks::default()),
            watcher_lifecycle: Arc::new(RwLock::new(())),
            statuses: Arc::new(RwLock::new(HashMap::new())),
            reconcile_lock: Arc::new(Mutex::new(())),
            hydration_slots: Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            hydration_complete: Arc::new(StdRwLock::new(None)),
        }
    }

    /// Invoked after native pending-message hydration has committed to the
    /// store. It is independent from webview event delivery.
    pub fn set_hydration_complete_hook(&self, hook: HydrationCompleteHook) {
        *self
            .hydration_complete
            .write()
            .expect("hydration completion hook lock poisoned") = Some(hook);
    }

    pub async fn reconcile(&self, app: AppHandle) -> anyhow::Result<()> {
        // Native startup and a mounted webview can request reconciliation at
        // nearly the same time. Serialize the replacement so the later call
        // cannot cancel half of the task set created by the earlier one.
        let _reconcile = self.reconcile_lock.lock().await;
        let _lifecycle = self.watcher_lifecycle.write().await;
        self.stop_all_inner().await;
        for account in self.store.accounts().await? {
            if account.enabled {
                self.start_account_inner(app.clone(), account).await;
            }
        }
        Ok(())
    }

    pub async fn start_account(&self, app: AppHandle, account: Account) {
        let _lifecycle = self.watcher_lifecycle.read().await;
        self.start_account_inner(app, account).await;
    }

    async fn start_account_inner(&self, app: AppHandle, account: Account) {
        let account_id = account.id;
        let _transition = account_task_lock(&self.account_task_locks, account_id).await;
        let (cancel, receiver) = watch::channel(false);
        let store = self.store.clone();
        let statuses = self.statuses.clone();
        let hydration_slots = self.hydration_slots.clone();
        let hydration_complete = self.hydration_complete.clone();
        replace_watcher_task(&self.tasks, account_id, async move {
            let handle = tauri::async_runtime::spawn(async move {
                run_watcher(
                    app,
                    store,
                    statuses,
                    account,
                    receiver,
                    hydration_slots,
                    hydration_complete,
                )
                .await;
            });
            WatcherTask { cancel, handle }
        })
        .await;
    }

    pub async fn stop_account(&self, account_id: Uuid) {
        let _lifecycle = self.watcher_lifecycle.read().await;
        let _transition = account_task_lock(&self.account_task_locks, account_id).await;
        self.stop_account_inner(account_id).await;
    }

    async fn stop_account_inner(&self, account_id: Uuid) {
        stop_watcher_task(self.tasks.lock().await.remove(&account_id)).await;
        self.statuses.write().await.remove(&account_id);
    }

    pub async fn stop_all(&self) {
        let _lifecycle = self.watcher_lifecycle.write().await;
        self.stop_all_inner().await;
    }

    async fn stop_all_inner(&self) {
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

async fn account_task_lock(
    account_task_locks: &AccountOperationLocks,
    account_id: Uuid,
) -> AccountOperationGuard {
    account_task_locks.acquire(account_id).await
}

async fn replace_watcher_task<Fut>(
    tasks: &Mutex<HashMap<Uuid, WatcherTask>>,
    account_id: Uuid,
    replacement: Fut,
) where
    Fut: Future<Output = WatcherTask>,
{
    stop_watcher_task(tasks.lock().await.remove(&account_id)).await;
    tasks.lock().await.insert(account_id, replacement.await);
}

async fn stop_watcher_task(task: Option<WatcherTask>) {
    if let Some(task) = task {
        let _ = task.cancel.send(true);
        let _ = task.handle.await;
    }
}

async fn run_watcher(
    app: AppHandle,
    store: Store,
    statuses: Arc<RwLock<HashMap<Uuid, RealtimeSyncStatus>>>,
    account: Account,
    cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
) {
    let sink: Arc<dyn RealtimeEventSink> = Arc::new(TauriRealtimeEventSink { app });
    let mut runner = MailServiceRealtimeCycleRunner {
        store: store.clone(),
        account: account.clone(),
    };
    let delay = TokioWatcherDelay;
    run_watcher_with_runner(
        sink,
        store,
        statuses,
        account,
        cancel,
        hydration_slots,
        hydration_complete,
        &mut runner,
        &delay,
        true,
    )
    .await;
}

/// Runs the same watcher state machine as production with a cycle runner at
/// the core boundary. Tests use this seam to make reconnect and cancellation
/// deterministic; production supplies `MailServiceRealtimeCycleRunner`.
#[allow(clippy::too_many_arguments)] // Test seam mirrors production watcher dependencies.
async fn run_watcher_with_runner<R: RealtimeCycleRunner, D: WatcherDelay>(
    sink: Arc<dyn RealtimeEventSink>,
    store: Store,
    statuses: Arc<RwLock<HashMap<Uuid, RealtimeSyncStatus>>>,
    account: Account,
    mut cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
    runner: &mut R,
    delay: &D,
    run_hydration: bool,
) {
    let mut failures = 0_usize;
    // Arrivals must never wait behind a long cache-warm pass. Keep their queue
    // separate and prioritize it in the worker; maintenance is coalesced to a
    // single pending pass because an extra empty IDLE renewal has no payload.
    let hydration = run_hydration.then(|| {
        let (arrival_sender, arrival_receiver) = mpsc::unbounded_channel::<HydrationBatch>();
        let (maintenance_sender, maintenance_receiver) =
            mpsc::channel::<HydrationBatch>(HYDRATION_MAINTENANCE_QUEUE_CAPACITY);
        let (cancel, cancel_receiver) = watch::channel(false);
        let handle = tauri::async_runtime::spawn(run_hydration_worker(
            sink.clone(),
            store.clone(),
            account.clone(),
            HydrationWorkerQueues {
                arrivals: arrival_receiver,
                maintenance: maintenance_receiver,
            },
            cancel_receiver,
            hydration_slots,
            hydration_complete,
        ));
        HydrationRuntime {
            arrival_sender,
            maintenance_sender,
            cancel,
            handle,
        }
    });
    let mut last_body_cache_warm = None;
    loop {
        if *cancel.borrow() {
            break;
        }
        publish_status(&*sink, &statuses, account.id, "connecting", None, None).await;
        let cycle_result = {
            let (idle_ready, mut idle_ready_events) = watch::channel(false);
            // The cycle receives its own cancellation view so the supervisor
            // can still drop a connect, authenticate, or reconcile future that
            // has not yet reached a cooperative cancellation point.
            let mut cycle_cancel = cancel.clone();
            let cycle = runner.next_cycle(&mut cycle_cancel, idle_ready);
            tokio::pin!(cycle);
            loop {
                tokio::select! {
                    biased;
                    result = &mut cycle => break result,
                    _ = wait_for_cancellation(&mut cancel) => {
                        break Ok(dakia_core::RealtimeCycle {
                            mode: RealtimeMode::Idle,
                            new_messages: Vec::new(),
                            catalogue_changed: false,
                            cancelled: true,
                            detected_at: None,
                            pending_hydration: Vec::new(),
                        });
                    }
                    ready = idle_ready_events.changed() => {
                        if ready.is_ok() && *idle_ready_events.borrow() {
                            publish_status(
                                &*sink,
                                &statuses,
                                account.id,
                                "idle",
                                None,
                                None,
                            )
                            .await;
                        }
                    }
                }
            }
        };
        match cycle_result {
            Ok(cycle) if cycle.cancelled => break,
            Ok(cycle) => {
                failures = 0;
                // IDLE is published only from the continuation callback above.
                // An IDLE-capable server can return an initial reconciliation
                // before it ever accepts IDLE, and that connection is closed
                // when this cycle completes.
                if cycle.mode == RealtimeMode::Poll {
                    publish_status(&*sink, &statuses, account.id, "polling", None, None).await;
                }
                if !cycle.new_messages.is_empty() {
                    let detected_at = cycle.detected_at.unwrap_or_else(chrono::Utc::now);
                    tracing::info!(
                        account_id = %account.id,
                        provider = %account.provider_id,
                        message_count = cycle.new_messages.len(),
                        "new mail headers persisted"
                    );
                    publish_event(
                        &*sink,
                        RealtimeEvent::Arrived,
                        MailArrival {
                            event_id: Uuid::new_v4(),
                            account_id: account.id,
                            messages: cycle.new_messages.clone(),
                            detected_at: detected_at.to_rfc3339(),
                        },
                    );
                }
                if cycle.catalogue_changed {
                    // A full INBOX reconciliation may only change flags or
                    // remove a message. There is no arrival payload in that
                    // case, so tell mounted views to reload their durable
                    // catalogue.
                    publish_event(
                        &*sink,
                        RealtimeEvent::Changed,
                        serde_json::json!({ "accountId": account.id }),
                    );
                }
                // Only a real arrival enters the unbounded priority queue. A
                // non-warm empty renewal is intentionally dropped, and a warm
                // request supersedes an already queued maintenance request.
                let now = Instant::now();
                let warm_recent = body_cache_warm_due(last_body_cache_warm, now);
                if warm_recent {
                    last_body_cache_warm = Some(now);
                }
                if !cycle.pending_hydration.is_empty() {
                    if let Some(hydration) = &hydration {
                        if hydration
                            .arrival_sender
                            .send(HydrationBatch {
                                pending: cycle.pending_hydration,
                                warm_recent: false,
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                account_id = %account.id,
                                "background hydration worker stopped unexpectedly"
                            );
                            break;
                        }
                    }
                }
                if warm_recent {
                    if let Some(hydration) = &hydration {
                        // Full means a maintenance pass is already queued. It is
                        // equivalent to this one, so coalescing preserves the
                        // cadence without letting the IDLE watcher block.
                        let _ = hydration.maintenance_sender.try_send(HydrationBatch {
                            pending: Vec::new(),
                            warm_recent: true,
                        });
                    }
                }
                if cycle.mode == RealtimeMode::Poll
                    && delay.wait(poll_delay(account.id), &mut cancel).await
                {
                    break;
                }
            }
            Err(error) => {
                if is_authentication_failure(&error) {
                    tracing::warn!(account_id = %account.id, provider = %account.provider_id, "real-time sync authentication paused");
                    publish_status(
                        &*sink,
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
                let reconnect_delay = retry_delay(failures, account.id);
                let retry_at = (chrono::Utc::now()
                    + chrono::Duration::from_std(reconnect_delay).unwrap_or_default())
                .to_rfc3339();
                tracing::warn!(
                    account_id = %account.id,
                    provider = %account.provider_id,
                    retry_seconds = reconnect_delay.as_secs(),
                    error = %error,
                    "real-time sync reconnect scheduled"
                );
                publish_status(
                    &*sink,
                    &statuses,
                    account.id,
                    "retrying",
                    Some(retry_at),
                    Some("connection"),
                )
                .await;
                if delay.wait(reconnect_delay, &mut cancel).await {
                    break;
                }
            }
        }
    }
    if let Some(hydration) = hydration {
        let _ = hydration.cancel.send(true);
        drop(hydration.arrival_sender);
        drop(hydration.maintenance_sender);
        let _ = hydration.handle.await;
    }
}

struct HydrationRuntime {
    arrival_sender: mpsc::UnboundedSender<HydrationBatch>,
    maintenance_sender: mpsc::Sender<HydrationBatch>,
    cancel: watch::Sender<bool>,
    handle: tauri::async_runtime::JoinHandle<()>,
}

fn is_authentication_failure(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("authentication")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HydrationKind {
    Pending,
    Recent,
    Starred,
}

#[derive(Clone)]
struct HydrationTarget {
    kind: HydrationKind,
    message: MailSummary,
}

struct HydrationBatch {
    pending: Vec<MailSummary>,
    warm_recent: bool,
}

struct HydrationWorkerQueues {
    arrivals: mpsc::UnboundedReceiver<HydrationBatch>,
    maintenance: mpsc::Receiver<HydrationBatch>,
}

struct HydrationResult {
    target: HydrationTarget,
    started: std::time::Instant,
    result: anyhow::Result<Option<MailSummary>>,
    cancelled: bool,
}

async fn pending_hydration_event(
    store: &Store,
    account_id: Uuid,
    outcome: &HydrationResult,
) -> Option<MailHydrated> {
    if outcome.cancelled || outcome.target.kind != HydrationKind::Pending {
        return None;
    }
    let hydrated = outcome.result.as_ref().ok()?.as_ref()?;
    if !same_hydration_receipt(&outcome.target.message, hydrated) {
        return None;
    }
    let durable = store.message(&hydrated.id).await.ok().flatten()?;
    if durable.account_id != account_id.to_string()
        || !same_hydration_receipt(&outcome.target.message, &durable)
    {
        return None;
    }
    Some(MailHydrated {
        account_id,
        message_id: hydrated.id.clone(),
    })
}

#[derive(Clone)]
struct HydrationContext {
    sink: Arc<dyn RealtimeEventSink>,
    store: Store,
    service: MailService,
    account: Account,
    slots: Arc<Semaphore>,
    complete: HydrationCompleteHookStore,
    recent_failures: Arc<Mutex<HashMap<String, Instant>>>,
    // Each worker walks a bounded window of the ordered candidate set. A
    // cooling newest prefix must not keep later messages beyond the first few
    // pages unreachable forever.
    recent_candidate_offset: Arc<Mutex<u32>>,
}

fn hydration_plan(
    pending: Vec<MailSummary>,
    recent: Vec<MailSummary>,
    starred: Vec<MailSummary>,
) -> Vec<HydrationTarget> {
    let mut seen = std::collections::HashSet::new();
    pending
        .into_iter()
        .map(|message| HydrationTarget {
            kind: HydrationKind::Pending,
            message,
        })
        .chain(recent.into_iter().map(|message| HydrationTarget {
            kind: HydrationKind::Recent,
            message,
        }))
        .chain(starred.into_iter().map(|message| HydrationTarget {
            kind: HydrationKind::Starred,
            message,
        }))
        .filter(|target| seen.insert(hydration_dedupe_key(&target.message)))
        .collect()
}

fn hydration_dedupe_key(message: &MailSummary) -> String {
    // MailService parses Message-ID values into their canonical form before
    // persistence. Lower-casing follows the storage/threading identity rules
    // and also handles rows that predate canonical storage. A blank or
    // malformed value is not an identity, so use the durable local id instead.
    match message
        .message_id
        .as_deref()
        .and_then(canonical_hydration_message_id)
    {
        Some(message_id) => format!("message-id:{message_id}"),
        _ => format!("id:{}", message.id),
    }
}

fn canonical_hydration_message_id(value: &str) -> Option<String> {
    let value = value.trim();
    let start = value.find('<')?;
    let end = value[start..].find('>')? + start;
    (end > start + 1).then(|| value[start..=end].to_ascii_lowercase())
}

fn body_cache_warm_due(last_warm: Option<Instant>, now: Instant) -> bool {
    last_warm
        .map(|last_warm| now.saturating_duration_since(last_warm) >= BODY_CACHE_WARM_INTERVAL)
        .unwrap_or(true)
}

fn select_recent_candidate_page(
    candidates: Vec<MailSummary>,
    failures: &mut HashMap<String, Instant>,
    selected: &mut Vec<MailSummary>,
) {
    let remaining = BODY_CACHE_WARM_BATCH_LIMIT.saturating_sub(selected.len() as u32) as usize;
    if remaining == 0 {
        return;
    }
    candidates
        .into_iter()
        .filter(|message| !failures.contains_key(&message.id))
        .take(remaining)
        .for_each(|message| selected.push(message));
}

fn next_recent_candidate_offset(
    start_offset: u32,
    pages_examined: u32,
    saw_empty_page: bool,
    batch_is_full: bool,
) -> u32 {
    if saw_empty_page || batch_is_full {
        0
    } else {
        start_offset.saturating_add(pages_examined.saturating_mul(BODY_CACHE_WARM_BATCH_LIMIT))
    }
}

async fn run_hydration_worker(
    sink: Arc<dyn RealtimeEventSink>,
    store: Store,
    account: Account,
    queues: HydrationWorkerQueues,
    cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
) {
    let service = MailService::new(store.clone());
    let context = HydrationContext {
        sink,
        store,
        service,
        account,
        slots: hydration_slots,
        complete: hydration_complete,
        recent_failures: Arc::new(Mutex::new(HashMap::new())),
        recent_candidate_offset: Arc::new(Mutex::new(0)),
    };
    run_prioritized_hydration_batches(
        queues.arrivals,
        queues.maintenance,
        cancel,
        move |batch, cancel| {
            let context = context.clone();
            async move {
                hydrate_after_sync(context, batch, cancel).await;
            }
        },
    )
    .await;
}

async fn run_prioritized_hydration_batches<T, F, Fut>(
    mut arrival_receiver: mpsc::UnboundedReceiver<T>,
    mut maintenance_receiver: mpsc::Receiver<T>,
    mut cancel: watch::Receiver<bool>,
    mut operation: F,
) where
    T: Send + 'static,
    F: FnMut(T, watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut arrivals_closed = false;
    let mut maintenance_closed = false;
    loop {
        if arrivals_closed && maintenance_closed {
            break;
        }
        let item = tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel) => break,
            item = arrival_receiver.recv(), if !arrivals_closed => {
                match item {
                    Some(item) => Some(item),
                    None => {
                        arrivals_closed = true;
                        None
                    }
                }
            },
            item = maintenance_receiver.recv(), if !maintenance_closed => {
                match item {
                    Some(item) => Some(item),
                    None => {
                        maintenance_closed = true;
                        None
                    }
                }
            },
        };
        let Some(item) = item else {
            continue;
        };
        operation(item, cancel.clone()).await;
    }
}

async fn hydrate_after_sync(
    context: HydrationContext,
    batch: HydrationBatch,
    cancel: watch::Receiver<bool>,
) {
    if *cancel.borrow() {
        return;
    }
    let recent_cutoff = chrono::Utc::now() - chrono::Duration::days(BODY_CACHE_WARM_LOOKBACK_DAYS);
    let recent = if batch.warm_recent {
        let refresh_cancel = cancel.clone();
        let refresh = context.service.refresh_recent_main_mailboxes(
            &context.account,
            recent_cutoff,
            BODY_CACHE_WARM_BATCH_LIMIT * 3,
        );
        let Some(refresh_result) = run_cancellable(refresh, refresh_cancel).await else {
            return;
        };
        match refresh_result {
            Ok(messages) if !messages.is_empty() => {
                publish_event(
                    &*context.sink,
                    RealtimeEvent::Changed,
                    serde_json::json!({ "accountId": context.account.id }),
                );
            }
            Ok(_) => {}
            Err(error) => {
                // Existing catalogue rows remain valid warm candidates. A
                // provider or UIDVALIDITY failure must not turn a cache
                // refresh into destructive catalogue recovery.
                tracing::warn!(
                    account_id = %context.account.id,
                    error = %error,
                    "could not refresh recent primary-folder headers before body-cache warming"
                );
            }
        }
        load_recent_warm_candidates(&context, recent_cutoff, &cancel).await
    } else {
        Vec::new()
    };
    let starred = match context
        .store
        .uncached_starred_messages(context.account.id, 25)
        .await
    {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(
                account_id = %context.account.id,
                error = %error,
                "could not load starred messages for background hydration"
            );
            Vec::new()
        }
    };
    // New arrivals take precedence, followed by recent primary-folder bodies,
    // then the durable starred backlog. `hydration_plan` deduplicates their
    // provider identity so a Gmail label duplicate is fetched once.
    let plan = hydration_plan(batch.pending, recent, starred);
    let hydration_account = context.account.clone();
    let hydration_context = context.clone();
    let results = run_bounded_cancellable_ordered(
        plan,
        HYDRATION_CONCURRENCY,
        context.slots,
        cancel,
        move |target, cancel| {
            let store = hydration_context.store.clone();
            let service = hydration_context.service.clone();
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
        if let Some(event) =
            pending_hydration_event(&context.store, context.account.id, &outcome).await
        {
            tracing::info!(
                account_id = %context.account.id,
                message_id = %event.message_id,
                hydration_ms = outcome.started.elapsed().as_millis(),
                "new mail hydration complete"
            );
            publish_event(&*context.sink, RealtimeEvent::Hydrated, event);
            notify_hydration_complete(&context.complete);
            continue;
        }
        match outcome.result {
            Ok(_) => {
                context
                    .recent_failures
                    .lock()
                    .await
                    .remove(&outcome.target.message.id);
            }
            Err(error) => {
                if outcome.target.kind == HydrationKind::Recent {
                    context.recent_failures.lock().await.insert(
                        outcome.target.message.id.clone(),
                        Instant::now() + BODY_CACHE_FAILURE_COOLDOWN,
                    );
                }
                tracing::warn!(
                    account_id = %context.account.id,
                    message_id = %outcome.target.message.id,
                    error = %error,
                    "background mail hydration failed"
                );
            }
        }
    }
}

async fn load_recent_warm_candidates(
    context: &HydrationContext,
    cutoff: chrono::DateTime<chrono::Utc>,
    cancel: &watch::Receiver<bool>,
) -> Vec<MailSummary> {
    let start_offset = *context.recent_candidate_offset.lock().await;
    let now = Instant::now();
    let mut failures = {
        let mut failures = context.recent_failures.lock().await;
        failures.retain(|_, retry_at| *retry_at > now);
        failures.clone()
    };
    let mut selected = Vec::new();
    let mut pages_examined = 0;
    let mut saw_empty_page = false;
    let mut page_query_failed = false;

    while pages_examined < BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE
        && selected.len() < BODY_CACHE_WARM_BATCH_LIMIT as usize
    {
        if *cancel.borrow() {
            return Vec::new();
        }
        let offset =
            start_offset.saturating_add(pages_examined.saturating_mul(BODY_CACHE_WARM_BATCH_LIMIT));
        let candidates = match context
            .store
            .recent_body_cache_candidates_page(
                context.account.id,
                cutoff,
                BODY_CACHE_WARM_BATCH_LIMIT,
                offset,
            )
            .await
        {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(
                    account_id = %context.account.id,
                    offset,
                    error = %error,
                    "could not load a recent body-cache candidate page"
                );
                page_query_failed = true;
                break;
            }
        };
        pages_examined += 1;
        if candidates.is_empty() {
            saw_empty_page = true;
            break;
        }
        select_recent_candidate_page(candidates, &mut failures, &mut selected);
    }

    let batch_is_full = selected.len() == BODY_CACHE_WARM_BATCH_LIMIT as usize;
    *context.recent_candidate_offset.lock().await = if page_query_failed {
        start_offset
    } else {
        next_recent_candidate_offset(start_offset, pages_examined, saw_empty_page, batch_is_full)
    };
    *context.recent_failures.lock().await = failures;
    selected
}

fn notify_hydration_complete(hydration_complete: &HydrationCompleteHookStore) {
    let hook = hydration_complete
        .read()
        .ok()
        .and_then(|hook| hook.as_ref().cloned());
    if let Some(hook) = hook {
        hook();
    }
}

async fn hydrate_target(
    store: Store,
    service: MailService,
    account: Account,
    target: HydrationTarget,
    mut cancel: watch::Receiver<bool>,
) -> HydrationResult {
    let started = Instant::now();
    if *cancel.borrow() {
        return HydrationResult {
            target,
            started,
            result: Ok(None),
            cancelled: true,
        };
    }

    let claim = match store
        .acquire_message_content_fetch(&target.message.id)
        .await
    {
        Ok(Some(claim)) => claim,
        // A foreground read owns the fetch/cache claim. It will either commit
        // the cache or release the claim for a later background cycle.
        Ok(None) => {
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
    };

    let (result, cancelled) = {
        let fetch_and_cache = fetch_and_cache_target(&store, &service, &account, &target);
        tokio::pin!(fetch_and_cache);
        tokio::select! {
            result = &mut fetch_and_cache => (result, false),
            _ = wait_for_cancellation(&mut cancel) => (Ok(None), true),
        }
    };
    if let Err(error) = claim.release().await {
        tracing::warn!(
            message_id = %target.message.id,
            error = %error,
            "could not release background body-cache fetch claim"
        );
    }
    HydrationResult {
        target,
        started,
        result,
        cancelled,
    }
}

async fn fetch_and_cache_target(
    store: &Store,
    service: &MailService,
    account: &Account,
    target: &HydrationTarget,
) -> anyhow::Result<Option<MailSummary>> {
    let message = service
        .fetch_message(account, &target.message.mailbox, target.message.uid as u32)
        .await?;
    cache_hydrated_message(store, &target.message, message).await
}

async fn cache_hydrated_message(
    store: &Store,
    target: &MailSummary,
    fetched: MailSummary,
) -> anyhow::Result<Option<MailSummary>> {
    if !same_hydration_receipt(target, &fetched) {
        return Ok(None);
    }
    if !store
        .publish_message_content_at_epoch(target, &fetched)
        .await?
    {
        return Ok(None);
    }
    // Publication is fenced atomically, but a later rollover or account
    // removal can finish before the event reaches the webview. Return only a
    // current durable row with the exact fetched receipt identity.
    Ok(store
        .message(&target.id)
        .await?
        .filter(|current| same_hydration_receipt(target, current)))
}

fn same_hydration_receipt(target: &MailSummary, message: &MailSummary) -> bool {
    target.id == message.id
        && target.account_id == message.account_id
        && target.mailbox == message.mailbox
        && target.uid == message.uid
        && target.mailbox_uid_validity.is_some()
        && target.mailbox_uid_validity == message.mailbox_uid_validity
}

async fn run_cancellable<F, T>(future: F, mut cancel: watch::Receiver<bool>) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => Some(result),
        _ = wait_for_cancellation(&mut cancel) => None,
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
            // Cancellation and a worker observing that cancellation can become
            // ready together. Prefer the coordinator signal so the next loop
            // cannot refill a newly vacant slot before recording the stop.
            biased;
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
    sink: &dyn RealtimeEventSink,
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
    let mut statuses = statuses.write().await;
    if statuses.get(&account_id) == Some(&status) {
        return;
    }
    statuses.insert(account_id, status.clone());
    drop(statuses);
    publish_event(sink, RealtimeEvent::SyncState, status);
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
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };
    use tokio::io::AsyncReadExt;

    #[path = "l010_tls_tests.rs"]
    mod l010_tls_tests;

    #[derive(Default)]
    struct RecordingEventSink {
        events: StdMutex<Vec<(&'static str, serde_json::Value)>>,
        changed: tokio::sync::Notify,
    }

    impl RealtimeEventSink for RecordingEventSink {
        fn publish(&self, event: RealtimeEvent, payload: serde_json::Value) {
            self.events.lock().unwrap().push((event.name(), payload));
            self.changed.notify_waiters();
        }
    }

    impl RecordingEventSink {
        fn events(&self) -> Vec<(&'static str, serde_json::Value)> {
            self.events.lock().unwrap().clone()
        }

        async fn wait_for_event(&self, event: &'static str) {
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if self.events().iter().any(|(name, _)| *name == event) {
                        return;
                    }
                    self.changed.notified().await;
                }
            })
            .await
            .expect("watcher should publish its production event through the sink");
        }
    }

    enum ScriptedCycleResult {
        Cycle(dakia_core::RealtimeCycle),
        Failure(&'static str),
        WaitForCancellation,
        IdleReadyThenWaitForCancellation,
        StalledTransport(tokio::net::TcpStream),
    }

    struct ScriptedCycleRunner {
        results: VecDeque<ScriptedCycleResult>,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptedCycleRunner {
        fn new(results: impl IntoIterator<Item = ScriptedCycleResult>) -> Self {
            Self {
                results: results.into_iter().collect(),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_counter(&self) -> Arc<AtomicUsize> {
            self.calls.clone()
        }
    }

    impl RealtimeCycleRunner for ScriptedCycleRunner {
        fn next_cycle<'a>(
            &'a mut self,
            cancel: &'a mut watch::Receiver<bool>,
            idle_ready: watch::Sender<bool>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<dakia_core::RealtimeCycle>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .results
                .pop_front()
                .expect("scripted watcher must provide a cycle result");
            Box::pin(async move {
                match result {
                    ScriptedCycleResult::Cycle(cycle) => Ok(cycle),
                    ScriptedCycleResult::Failure(message) => Err(anyhow::anyhow!(message)),
                    ScriptedCycleResult::WaitForCancellation => {
                        wait_for_cancellation(cancel).await;
                        Ok(dakia_core::RealtimeCycle {
                            mode: RealtimeMode::Idle,
                            new_messages: Vec::new(),
                            catalogue_changed: false,
                            cancelled: true,
                            detected_at: None,
                            pending_hydration: Vec::new(),
                        })
                    }
                    ScriptedCycleResult::IdleReadyThenWaitForCancellation => {
                        let _ = idle_ready.send(true);
                        wait_for_cancellation(cancel).await;
                        Ok(dakia_core::RealtimeCycle {
                            mode: RealtimeMode::Idle,
                            new_messages: Vec::new(),
                            catalogue_changed: false,
                            cancelled: true,
                            detected_at: None,
                            pending_hydration: Vec::new(),
                        })
                    }
                    ScriptedCycleResult::StalledTransport(_transport) => {
                        std::future::pending::<()>().await;
                        unreachable!("stalled transport is released only by dropping its cycle")
                    }
                }
            })
        }
    }

    #[derive(Clone, Copy)]
    enum ControlledDelayStep {
        Continue,
        WaitForCancellation,
    }

    struct ControlledWatcherDelay {
        steps: StdMutex<VecDeque<ControlledDelayStep>>,
        requested: Arc<StdMutex<Vec<Duration>>>,
        entered: mpsc::UnboundedSender<Duration>,
    }

    impl ControlledWatcherDelay {
        fn new(
            steps: impl IntoIterator<Item = ControlledDelayStep>,
        ) -> (
            Self,
            Arc<StdMutex<Vec<Duration>>>,
            mpsc::UnboundedReceiver<Duration>,
        ) {
            let (entered, receiver) = mpsc::unbounded_channel();
            let requested = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    steps: StdMutex::new(steps.into_iter().collect()),
                    requested: requested.clone(),
                    entered,
                },
                requested,
                receiver,
            )
        }
    }

    impl WatcherDelay for ControlledWatcherDelay {
        fn wait<'a>(
            &'a self,
            delay: Duration,
            cancel: &'a mut watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            self.requested.lock().unwrap().push(delay);
            self.entered
                .send(delay)
                .expect("controlled delay observer remains available");
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("every watcher delay must have a controlled outcome");
            Box::pin(async move {
                match step {
                    ControlledDelayStep::Continue => false,
                    ControlledDelayStep::WaitForCancellation => {
                        wait_for_cancellation(cancel).await;
                        true
                    }
                }
            })
        }
    }

    fn realtime_account() -> Account {
        dakia_core::AccountDraft {
            email: "realtime@example.test".into(),
            display_name: "Realtime".into(),
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
        .into_account(dakia_core::provider::by_id("fastmail").unwrap())
    }

    fn cancelled_cycle() -> dakia_core::RealtimeCycle {
        dakia_core::RealtimeCycle {
            mode: RealtimeMode::Idle,
            new_messages: Vec::new(),
            catalogue_changed: false,
            cancelled: true,
            detected_at: None,
            pending_hydration: Vec::new(),
        }
    }

    fn message(id: &str) -> MailSummary {
        MailSummary {
            mailbox_uid_validity: None,
            id: id.into(),
            account_id: Uuid::nil().to_string(),
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: None,
            gmail_message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: id.into(),
            subject: id.into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
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
            attachments: Vec::new(),
        }
    }

    fn tracked_watcher(running: Arc<AtomicUsize>) -> WatcherTask {
        let (cancel, mut receiver) = watch::channel(false);
        running.fetch_add(1, Ordering::SeqCst);
        let handle = tauri::async_runtime::spawn(async move {
            wait_for_cancellation(&mut receiver).await;
            running.fetch_sub(1, Ordering::SeqCst);
        });
        WatcherTask { cancel, handle }
    }

    async fn replace_test_watcher(
        account_task_locks: Arc<AccountOperationLocks>,
        tasks: Arc<Mutex<HashMap<Uuid, WatcherTask>>>,
        account_id: Uuid,
        start_barrier: Arc<tokio::sync::Barrier>,
        entered_transition: mpsc::Sender<()>,
        mut release_replacement: watch::Receiver<bool>,
        running: Arc<AtomicUsize>,
    ) {
        start_barrier.wait().await;
        let _transition = account_task_lock(&account_task_locks, account_id).await;
        entered_transition.send(()).await.unwrap();
        replace_watcher_task(&tasks, account_id, async move {
            wait_for_cancellation(&mut release_replacement).await;
            tracked_watcher(running)
        })
        .await;
    }

    #[tokio::test]
    async fn production_watcher_loop_publishes_arrivals_through_the_event_sink() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut arrived = message("arrived-from-realtime");
        arrived.account_id = account.id.to_string();
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Cycle(dakia_core::RealtimeCycle {
                mode: RealtimeMode::Idle,
                new_messages: vec![arrived.clone()],
                catalogue_changed: false,
                cancelled: false,
                detected_at: Some(Utc::now()),
                pending_hydration: Vec::new(),
            }),
            ScriptedCycleResult::Cycle(cancelled_cycle()),
        ]);
        let (_cancel, receiver) = watch::channel(false);

        run_watcher_with_runner(
            sink.clone(),
            store,
            statuses.clone(),
            account.clone(),
            receiver,
            Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            Arc::new(StdRwLock::new(None)),
            &mut runner,
            &TokioWatcherDelay,
            false,
        )
        .await;

        let events = sink.events();
        assert_eq!(
            events.iter().map(|(event, _)| *event).collect::<Vec<_>>(),
            vec!["mail-sync-state", "mail-arrived"],
            "the watcher must publish its status and arrival through the same sink used by Tauri"
        );
        let (_, arrival) = events
            .iter()
            .find(|(event, _)| *event == "mail-arrived")
            .unwrap();
        assert_eq!(arrival["accountId"], account.id.to_string());
        assert_eq!(arrival["messages"][0]["id"], arrived.id);
        assert_eq!(arrival["messages"][0]["account_id"], account.id.to_string());
        assert_eq!(statuses.read().await[&account.id].state, "connecting");
    }

    #[tokio::test]
    async fn idle_ready_progress_is_visible_before_cycle_completion_without_duplicate_status() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let account_id = account.id;
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let (cancel, receiver) = watch::channel(false);
        let task_sink = sink.clone();
        let task_statuses = statuses.clone();
        let task = tokio::spawn(async move {
            let mut runner =
                ScriptedCycleRunner::new([ScriptedCycleResult::IdleReadyThenWaitForCancellation]);
            run_watcher_with_runner(
                task_sink,
                store,
                task_statuses,
                account,
                receiver,
                Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                Arc::new(StdRwLock::new(None)),
                &mut runner,
                &TokioWatcherDelay,
                false,
            )
            .await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if statuses
                    .read()
                    .await
                    .get(&account_id)
                    .is_some_and(|status| status.state == "idle")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("IDLE continuation should publish readiness before the cycle returns");
        cancel.send(true).unwrap();
        task.await.unwrap();

        let states = sink
            .events()
            .into_iter()
            .filter(|(event, _)| *event == "mail-sync-state")
            .map(|(_, payload)| payload["state"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(states, vec!["connecting", "idle"]);
    }

    #[tokio::test]
    async fn idle_capability_without_an_accepted_idle_continuation_never_publishes_idle() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut arrived = message("initial-reconciliation");
        arrived.account_id = account.id.to_string();
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Cycle(dakia_core::RealtimeCycle {
                mode: RealtimeMode::Idle,
                new_messages: vec![arrived],
                catalogue_changed: false,
                cancelled: false,
                detected_at: Some(Utc::now()),
                pending_hydration: Vec::new(),
            }),
            ScriptedCycleResult::Cycle(cancelled_cycle()),
        ]);
        let (_cancel, receiver) = watch::channel(false);

        run_watcher_with_runner(
            sink.clone(),
            store,
            statuses.clone(),
            account.clone(),
            receiver,
            Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            Arc::new(StdRwLock::new(None)),
            &mut runner,
            &TokioWatcherDelay,
            false,
        )
        .await;

        let states = sink
            .events()
            .into_iter()
            .filter(|(event, _)| *event == "mail-sync-state")
            .map(|(_, payload)| payload["state"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(states, ["connecting"]);
        assert_eq!(statuses.read().await[&account.id].state, "connecting");
    }

    #[tokio::test]
    async fn cancellation_drops_a_stalled_cycle_transport_before_watcher_exit() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled lifecycle listener");
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .expect("connect stalled lifecycle transport");
        let (mut peer, _) = listener
            .accept()
            .await
            .expect("accept stalled lifecycle transport");
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let (cancel, receiver) = watch::channel(false);
        let task = tokio::spawn({
            let task_sink = sink.clone();
            let task_statuses = statuses.clone();
            async move {
                let mut runner =
                    ScriptedCycleRunner::new([ScriptedCycleResult::StalledTransport(client)]);
                run_watcher_with_runner(
                    task_sink,
                    store,
                    task_statuses,
                    account,
                    receiver,
                    Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                    Arc::new(StdRwLock::new(None)),
                    &mut runner,
                    &TokioWatcherDelay,
                    false,
                )
                .await;
            }
        });
        sink.wait_for_event("mail-sync-state").await;

        cancel.send(true).expect("cancel stalled production cycle");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("watcher must not wait for a command timeout")
            .expect("stalled watcher task");
        let mut byte = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer.read(&mut byte))
                .await
                .expect("peer must observe prompt transport close")
                .expect("read stalled transport EOF"),
            0,
            "dropping the in-flight cycle must close its transport before watcher exit"
        );
    }

    #[tokio::test]
    async fn production_tauri_mock_runtime_sink_serializes_the_watcher_arrival() {
        use tauri::Listener;

        let app = tauri::test::mock_app();
        let (payload_sender, payload_receiver) = std::sync::mpsc::channel();
        let (cancel, receiver) = watch::channel(false);
        let cancellation = cancel.clone();
        app.listen("mail-arrived", move |event| {
            payload_sender
                .send(event.payload().to_owned())
                .expect("test receiver remains available");
            // The runtime-produced cycle is polling. Cancel at the same
            // production event boundary so this test exercises the watcher
            // transition without sleeping for the provider poll interval.
            let _ = cancellation.send(true);
        });
        let store = Store::in_memory().await.unwrap();
        let core_cycle = std::env::var("DAKIA_REALTIME_DELIVERY_CORE_ARTIFACT")
            .ok()
            .map(|path| {
                let artifact: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(path).expect("read scripted provider artifact"),
                )
                .expect("parse scripted provider artifact");
                let nonce = std::env::var("DAKIA_REALTIME_DELIVERY_NONCE")
                    .expect("orchestrated delivery proof supplies a nonce");
                assert_eq!(artifact["nonce"], nonce);
                let cycle = &artifact["cycle"];
                let mode = match cycle["mode"].as_str() {
                    Some("idle") => RealtimeMode::Idle,
                    Some("poll") => RealtimeMode::Poll,
                    other => panic!("unknown runtime-produced realtime mode: {other:?}"),
                };
                dakia_core::RealtimeCycle {
                    mode,
                    new_messages: serde_json::from_value(cycle["newMessages"].clone())
                        .expect("decode runtime-produced realtime arrivals"),
                    catalogue_changed: cycle["catalogueChanged"]
                        .as_bool()
                        .expect("runtime-produced catalogue change flag"),
                    cancelled: cycle["cancelled"]
                        .as_bool()
                        .expect("runtime-produced cancellation flag"),
                    detected_at: serde_json::from_value(cycle["detectedAt"].clone())
                        .expect("decode runtime-produced detection time"),
                    pending_hydration: serde_json::from_value(cycle["pendingHydration"].clone())
                        .expect("decode runtime-produced hydration targets"),
                }
            });
        let mut account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let cycle = core_cycle.unwrap_or_else(|| {
            let mut arrived = message("runtime-arrival");
            arrived.account_id = account.id.to_string();
            arrived.mailbox_uid_validity = Some(424242);
            arrived.subject = "Runtime emitted arrival".into();
            dakia_core::RealtimeCycle {
                mode: RealtimeMode::Idle,
                new_messages: vec![arrived],
                catalogue_changed: false,
                cancelled: false,
                detected_at: Some(Utc::now()),
                pending_hydration: Vec::new(),
            }
        });
        let expected = cycle
            .new_messages
            .first()
            .cloned()
            .expect("scripted provider cycle returns an arrival");
        account.id =
            Uuid::parse_str(&expected.account_id).expect("core cycle arrival has an account UUID");
        let mut runner = ScriptedCycleRunner::new([ScriptedCycleResult::Cycle(cycle)]);
        let sink: Arc<dyn RealtimeEventSink> = Arc::new(TauriRealtimeEventSink {
            app: app.handle().clone(),
        });

        tokio::time::timeout(
            Duration::from_secs(1),
            run_watcher_with_runner(
                sink,
                store,
                statuses,
                account.clone(),
                receiver,
                Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                Arc::new(StdRwLock::new(None)),
                &mut runner,
                &TokioWatcherDelay,
                // The coupled case verifies only header arrival publication.
                // Do not spawn body workers from a scripted provider cycle.
                false,
            ),
        )
        .await
        .expect("watcher must stop at the emitted arrival cancellation boundary");

        let payload = payload_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the production Tauri emitter must receive watcher events");
        let arrival: serde_json::Value = serde_json::from_str(&payload)
            .expect("the exact Tauri-emitted payload must remain frontend-decodable");
        assert_eq!(arrival["accountId"], account.id.to_string());
        assert_eq!(arrival["messages"][0]["id"], expected.id);
        assert_eq!(
            arrival["messages"][0]["mailbox_uid_validity"],
            serde_json::json!(expected.mailbox_uid_validity)
        );
        assert_eq!(arrival["messages"][0]["subject"], expected.subject);

        if let Ok(path) = std::env::var("DAKIA_REALTIME_DELIVERY_ARTIFACT") {
            let nonce = std::env::var("DAKIA_REALTIME_DELIVERY_NONCE")
                .expect("orchestrated delivery proof supplies a nonce");
            std::fs::write(
                path,
                serde_json::json!({ "nonce": nonce, "arrival": arrival }).to_string(),
            )
            .expect("write runtime-captured frontend delivery artifact");
        }
    }

    #[tokio::test]
    async fn production_watcher_loop_publishes_catalogue_changes_without_an_arrival() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Cycle(dakia_core::RealtimeCycle {
                mode: RealtimeMode::Idle,
                new_messages: Vec::new(),
                catalogue_changed: true,
                cancelled: false,
                detected_at: None,
                pending_hydration: Vec::new(),
            }),
            ScriptedCycleResult::Cycle(cancelled_cycle()),
        ]);
        let (_cancel, receiver) = watch::channel(false);

        run_watcher_with_runner(
            sink.clone(),
            store,
            statuses,
            account.clone(),
            receiver,
            Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            Arc::new(StdRwLock::new(None)),
            &mut runner,
            &TokioWatcherDelay,
            false,
        )
        .await;

        let events = sink.events();
        assert_eq!(
            events.iter().map(|(event, _)| *event).collect::<Vec<_>>(),
            vec!["mail-sync-state", "mail-changed"],
            "flag or deletion reconciliation must reload the frontend even when no message arrived"
        );
        let (_, changed) = events
            .iter()
            .find(|(event, _)| *event == "mail-changed")
            .unwrap();
        assert_eq!(changed["accountId"], account.id.to_string());
    }

    #[tokio::test]
    async fn uidvalidity_reconciliation_failure_publishes_retry_and_cancellation_stops_the_loop() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let (cancel, receiver) = watch::channel(false);
        let task = tokio::spawn({
            let sink = sink.clone();
            let statuses = statuses.clone();
            let account = account.clone();
            async move {
                let mut runner = ScriptedCycleRunner::new([
                    ScriptedCycleResult::Failure("UIDVALIDITY changed during reconciliation"),
                    ScriptedCycleResult::WaitForCancellation,
                ]);
                run_watcher_with_runner(
                    sink,
                    store,
                    statuses,
                    account,
                    receiver,
                    Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                    Arc::new(StdRwLock::new(None)),
                    &mut runner,
                    &TokioWatcherDelay,
                    false,
                )
                .await;
            }
        });

        sink.wait_for_event("mail-sync-state").await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if statuses
                    .read()
                    .await
                    .get(&account.id)
                    .is_some_and(|status| status.state == "retrying")
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("UIDVALIDITY failures should enter the reconnect state");
        cancel.send(true).unwrap();
        task.await.unwrap();

        let events = sink.events();
        assert_eq!(
            events.iter().map(|(event, _)| *event).collect::<Vec<_>>(),
            vec!["mail-sync-state", "mail-sync-state"],
            "cancellation during backoff must not start another cycle or publish stale data"
        );
        let retry = &events[1].1;
        assert_eq!(retry["state"], "retrying");
        assert_eq!(retry["errorKind"], "connection");
        assert!(retry["retryAt"].as_str().is_some());
    }

    #[tokio::test]
    async fn transient_and_rate_limit_failures_wait_in_bounded_order_without_a_reconnect_storm() {
        let store = Store::in_memory().await.unwrap();
        let mut account = realtime_account();
        account.id = Uuid::from_bytes([0; 16]);
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Failure("provider 429 rate limit"),
            ScriptedCycleResult::Failure("connection reset"),
            ScriptedCycleResult::Failure("provider temporarily unavailable"),
            ScriptedCycleResult::Failure("idle connection closed"),
            ScriptedCycleResult::Failure("network timeout"),
            ScriptedCycleResult::Failure("provider 429 rate limit"),
            ScriptedCycleResult::Cycle(cancelled_cycle()),
        ]);
        let calls = runner.call_counter();
        let (delay, requested, mut entered) = ControlledWatcherDelay::new([
            ControlledDelayStep::Continue,
            ControlledDelayStep::Continue,
            ControlledDelayStep::Continue,
            ControlledDelayStep::Continue,
            ControlledDelayStep::Continue,
            ControlledDelayStep::Continue,
        ]);
        let (_cancel, receiver) = watch::channel(false);

        run_watcher_with_runner(
            sink.clone(),
            store,
            statuses.clone(),
            account.clone(),
            receiver,
            Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            Arc::new(StdRwLock::new(None)),
            &mut runner,
            &delay,
            false,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 7);
        assert_eq!(
            *requested.lock().unwrap(),
            [
                Duration::from_millis(900),
                Duration::from_millis(1_800),
                Duration::from_millis(4_500),
                Duration::from_secs(9),
                Duration::from_secs(27),
                Duration::from_secs(54),
            ],
            "every failed connection must pass through the capped production retry schedule"
        );
        let mut observed = Vec::new();
        while let Ok(delay) = entered.try_recv() {
            observed.push(delay);
        }
        assert_eq!(observed, *requested.lock().unwrap());

        let events = sink.events();
        assert_eq!(events.len(), 13);
        for failure in 0..6 {
            let connecting = &events[failure * 2].1;
            let retrying = &events[failure * 2 + 1].1;
            assert_eq!(connecting["state"], "connecting");
            assert_eq!(connecting["errorKind"], serde_json::Value::Null);
            assert_eq!(retrying["state"], "retrying");
            assert_eq!(retrying["errorKind"], "connection");
            assert!(retrying["retryAt"].as_str().is_some());
        }
        assert_eq!(events.last().unwrap().1["state"], "connecting");
        assert_eq!(statuses.read().await[&account.id].state, "connecting");
    }

    #[tokio::test]
    async fn cancellation_inside_retry_delay_prevents_the_next_provider_attempt() {
        let store = Store::in_memory().await.unwrap();
        let mut account = realtime_account();
        account.id = Uuid::from_bytes([0; 16]);
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Failure("provider 429 rate limit"),
            ScriptedCycleResult::Failure("must never be attempted"),
        ]);
        let calls = runner.call_counter();
        let (delay, requested, mut entered) =
            ControlledWatcherDelay::new([ControlledDelayStep::WaitForCancellation]);
        let (cancel, receiver) = watch::channel(false);

        let watcher = tokio::spawn(async move {
            run_watcher_with_runner(
                sink.clone(),
                store,
                statuses,
                account,
                receiver,
                Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                Arc::new(StdRwLock::new(None)),
                &mut runner,
                &delay,
                false,
            )
            .await;
            sink.events()
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), entered.recv())
                .await
                .expect("watcher must enter its retry barrier"),
            Some(Duration::from_millis(900))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cancel.send(true).unwrap();
        let events = tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("cancellation must release the retry barrier")
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*requested.lock().unwrap(), [Duration::from_millis(900)]);
        assert_eq!(
            events.iter().map(|(event, _)| *event).collect::<Vec<_>>(),
            ["mail-sync-state", "mail-sync-state"]
        );
        assert_eq!(events[0].1["state"], "connecting");
        assert_eq!(events[1].1["state"], "retrying");
    }

    #[tokio::test]
    async fn permanent_authentication_failure_pauses_without_retrying_credentials() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let sink = Arc::new(RecordingEventSink::default());
        let mut runner = ScriptedCycleRunner::new([
            ScriptedCycleResult::Failure("IMAP authentication rejected"),
            ScriptedCycleResult::Failure("must never retry credentials"),
        ]);
        let calls = runner.call_counter();
        let (delay, requested, _entered) =
            ControlledWatcherDelay::new(std::iter::empty::<ControlledDelayStep>());
        let (_cancel, receiver) = watch::channel(false);

        run_watcher_with_runner(
            sink.clone(),
            store,
            statuses.clone(),
            account.clone(),
            receiver,
            Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
            Arc::new(StdRwLock::new(None)),
            &mut runner,
            &delay,
            false,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(requested.lock().unwrap().is_empty());
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1["state"], "connecting");
        assert_eq!(events[1].1["state"], "paused");
        assert_eq!(events[1].1["errorKind"], "authentication");
        assert_eq!(events[1].1["retryAt"], serde_json::Value::Null);
        let final_status = statuses.read().await[&account.id].clone();
        assert_eq!(final_status.state, "paused");
        assert_eq!(final_status.error_kind.as_deref(), Some("authentication"));
        assert!(final_status.retry_at.is_none());
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered() {
        // These account IDs select the two deterministic jitter extremes:
        // byte 0 gives -10%, and byte 20 gives +10%.
        let earlier = Uuid::from_bytes([0; 16]);
        let later = Uuid::from_bytes([20; 16]);

        assert_eq!(retry_delay(1, earlier), Duration::from_millis(900));
        assert_eq!(retry_delay(1, later), Duration::from_millis(1_100));
        assert_ne!(retry_delay(1, earlier), retry_delay(1, later));

        // Failures cap at the six-step, 60-second production backoff before
        // jitter. Both account-specific delays remain below the 66-second
        // reconnect ceiling, even for an unbounded failure count.
        assert_eq!(retry_delay(100, earlier), retry_delay(6, earlier));
        assert_eq!(retry_delay(100, later), retry_delay(6, later));
        assert_eq!(retry_delay(100, earlier), Duration::from_secs(54));
        assert_eq!(retry_delay(100, later), Duration::from_secs(66));
        assert!(retry_delay(100, later) <= Duration::from_secs(66));
    }

    #[test]
    fn legacy_oauth_authentication_failures_pause_while_token_service_failures_retry() {
        assert!(is_authentication_failure(&anyhow::anyhow!(
            "OAuth authentication failed (400 Bad Request): invalid_grant"
        )));
        assert!(!is_authentication_failure(&anyhow::anyhow!(
            "OAuth token refresh failed (503 Service Unavailable)"
        )));
    }

    #[test]
    fn fallback_polling_is_five_minutes_with_account_jitter() {
        let delay = poll_delay(Uuid::from_bytes([20; 16]));
        assert!((Duration::from_secs(270)..=Duration::from_secs(330)).contains(&delay));
    }

    #[test]
    fn hydration_plan_prioritizes_pending_recent_and_starred_by_canonical_identity() {
        let mut pending_duplicate = message("pending-duplicate");
        pending_duplicate.message_id = Some("<SAME@example.test>".into());
        let mut recent_duplicate = message("recent-duplicate");
        recent_duplicate.message_id = Some("<same@example.test>".into());
        let mut recent = message("recent-1");
        recent.message_id = Some("<recent@example.test>".into());
        let plan = hydration_plan(
            vec![message("pending-1"), pending_duplicate],
            vec![recent_duplicate, recent],
            vec![message("pending-1"), message("starred-1")],
        );
        assert_eq!(
            plan.iter()
                .map(|target| (target.message.id.as_str(), target.kind))
                .collect::<Vec<_>>(),
            vec![
                ("pending-1", HydrationKind::Pending),
                ("pending-duplicate", HydrationKind::Pending),
                ("recent-1", HydrationKind::Recent),
                ("starred-1", HydrationKind::Starred),
            ]
        );
    }

    #[test]
    fn body_cache_warming_runs_immediately_then_every_fifteen_minutes() {
        let start = Instant::now();
        assert!(body_cache_warm_due(None, start));
        assert!(!body_cache_warm_due(
            Some(start),
            start + BODY_CACHE_WARM_INTERVAL - Duration::from_secs(1),
        ));
        assert!(body_cache_warm_due(
            Some(start),
            start + BODY_CACHE_WARM_INTERVAL,
        ));
    }

    #[test]
    fn recent_body_failures_advance_past_more_than_twelve_hundred_cooling_messages() {
        let now = Instant::now();
        let cooling_count =
            BODY_CACHE_WARM_BATCH_LIMIT * BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE * 5;
        let candidates = (0..=cooling_count)
            .map(|index| message(&format!("recent-{index}")))
            .collect::<Vec<_>>();
        let mut failures = (0..cooling_count)
            .map(|index| (format!("recent-{index}"), now + BODY_CACHE_FAILURE_COOLDOWN))
            .collect::<HashMap<_, _>>();
        let mut offset = 0;
        let window = BODY_CACHE_WARM_BATCH_LIMIT * BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE;

        // Five bounded warm cycles traverse 1,500 cooling rows without ever
        // scheduling one. The persisted cursor advances by one page window
        // each time instead of repeatedly re-querying the newest 300 rows.
        while offset < cooling_count {
            let start_offset = offset;
            let mut selected = Vec::new();
            for page in 0..BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE {
                let page_offset = start_offset + page * BODY_CACHE_WARM_BATCH_LIMIT;
                let page = candidates
                    .iter()
                    .skip(page_offset as usize)
                    .take(BODY_CACHE_WARM_BATCH_LIMIT as usize)
                    .cloned()
                    .collect();
                select_recent_candidate_page(page, &mut failures, &mut selected);
            }
            assert!(selected.is_empty());
            offset = next_recent_candidate_offset(
                start_offset,
                BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE,
                false,
                false,
            );
            assert_eq!(offset, start_offset + window);
        }

        let start_offset = offset;
        let mut selected = Vec::new();
        let mut pages_examined = 0;
        let mut saw_empty_page = false;
        for page in 0..BODY_CACHE_WARM_CANDIDATE_PAGES_PER_CYCLE {
            let page_offset = start_offset + page * BODY_CACHE_WARM_BATCH_LIMIT;
            let page = candidates
                .iter()
                .skip(page_offset as usize)
                .take(BODY_CACHE_WARM_BATCH_LIMIT as usize)
                .cloned()
                .collect::<Vec<_>>();
            pages_examined += 1;
            if page.is_empty() {
                saw_empty_page = true;
                break;
            }
            select_recent_candidate_page(page, &mut failures, &mut selected);
        }

        let expected = format!("recent-{cooling_count}");
        assert_eq!(
            selected
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec![expected.as_str()]
        );
        assert!(saw_empty_page);
        assert_eq!(
            next_recent_candidate_offset(start_offset, pages_examined, saw_empty_page, false),
            0,
            "an empty page wraps the worker cursor for the next warm cycle"
        );
    }

    #[tokio::test]
    async fn cancellation_drops_a_stalled_recent_header_refresh() {
        let (cancel, receiver) = watch::channel(false);
        cancel.send(true).unwrap();
        assert!(run_cancellable(std::future::pending::<()>(), receiver)
            .await
            .is_none());

        let (_cancel, receiver) = watch::channel(false);
        assert_eq!(run_cancellable(async { 42 }, receiver).await, Some(42));
    }

    async fn save_hydration_target(
        store: &Store,
        account: &Account,
        id: &str,
        uid_validity: u32,
    ) -> MailSummary {
        store.save_account(account).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", uid_validity, 1, true)
            .await
            .unwrap();
        let mut target = message(id);
        target.account_id = account.id.to_string();
        target.uid = 42;
        target.mailbox_uid_validity = Some(i64::from(uid_validity));
        store
            .upsert_messages(std::slice::from_ref(&target))
            .await
            .unwrap();
        target
    }

    #[tokio::test]
    async fn stale_hydration_cannot_attach_a_recycled_uidvalidity_body_or_emit_completion() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let target = save_hydration_target(&store, &account, "recycled-uid", 77).await;
        let mut fetched = target.clone();
        fetched.body_text = "old generation body".into();
        fetched.content_state = "complete".into();
        fetched.has_attachments = true;
        fetched
            .attachments
            .push(dakia_core::storage::AttachmentData {
                attachment: dakia_core::Attachment {
                    id: "old-generation-paperclip".into(),
                    message_id: target.id.clone(),
                    filename: "old-generation.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size_bytes: 42,
                    size_kind: dakia_core::AttachmentSizeKind::Decoded,
                    is_inline: false,
                    presentation: dakia_core::AttachmentPresentation::Downloadable,
                    is_potentially_unsafe: false,
                },
                bytes: Vec::new(),
            });

        // The same stable local ID and UID now identify the replacement
        // generation. The old receipt must be unable to publish into it.
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 78, 1, true)
            .await
            .unwrap();
        let mut replacement = target.clone();
        replacement.mailbox_uid_validity = Some(78);
        replacement.subject = "replacement generation".into();
        replacement.body_text = String::new();
        replacement.content_state = "headers_only".into();
        replacement.has_attachments = false;
        store
            .upsert_messages(std::slice::from_ref(&replacement))
            .await
            .unwrap();

        let result = cache_hydrated_message(&store, &target, fetched)
            .await
            .unwrap();
        assert!(result.is_none());
        let current = store.message(&target.id).await.unwrap().unwrap();
        assert_eq!(current.mailbox_uid_validity, Some(78));
        assert_eq!(current.subject, "replacement generation");
        assert_eq!(current.body_text, "");
        assert_eq!(current.content_state, "headers_only");
        assert!(!current.has_attachments);
        assert!(current.attachments.is_empty());
        assert!(pending_hydration_event(
            &store,
            account.id,
            &HydrationResult {
                target: HydrationTarget {
                    kind: HydrationKind::Pending,
                    message: target,
                },
                started: Instant::now(),
                result: Ok(result),
                cancelled: false,
            },
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn current_hydration_returns_the_canonical_epoch_matched_row() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let target = save_hydration_target(&store, &account, "current-uid", 78).await;
        let mut fetched = target.clone();
        fetched.body_text = "current generation body".into();
        fetched.content_state = "complete".into();

        let current = cache_hydrated_message(&store, &target, fetched)
            .await
            .unwrap()
            .expect("the captured current generation should publish");
        assert!(same_hydration_receipt(&target, &current));
        // The catalogue projection deliberately leaves display bodies out of
        // `messages`; publication stores the fetched body in the bounded
        // cache used by the Reader. The event needs the canonical receipt,
        // while this assertion proves the body was not silently discarded.
        assert_eq!(current.content_state, "complete");
        assert_eq!(current.body_text, "");
        let cached = store
            .cached_message_content(&target.id)
            .await
            .expect("read regular content cache")
            .expect("successful hydration must retain display content");
        assert_eq!(cached.body_text, "current generation body");
    }

    #[tokio::test]
    async fn late_pending_hydration_does_not_emit_for_a_recycled_uidvalidity_row() {
        let store = Store::in_memory().await.unwrap();
        let account = realtime_account();
        let target = save_hydration_target(&store, &account, "late-recycled-uid", 77).await;
        let mut fetched = target.clone();
        fetched.body_text = "old body".into();
        fetched.content_state = "complete".into();
        let cached = cache_hydrated_message(&store, &target, fetched)
            .await
            .unwrap()
            .expect("initial generation cache must publish before the late-event race");

        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 78, 1, true)
            .await
            .unwrap();
        let mut replacement = target.clone();
        replacement.mailbox_uid_validity = Some(78);
        replacement.subject = "replacement mail".into();
        replacement.body_text = String::new();
        replacement.content_state = "headers_only".into();
        store.upsert_messages(&[replacement]).await.unwrap();

        assert!(
            pending_hydration_event(
                &store,
                account.id,
                &HydrationResult {
                    target: HydrationTarget {
                        kind: HydrationKind::Pending,
                        message: target,
                    },
                    started: Instant::now(),
                    result: Ok(Some(cached)),
                    cancelled: false,
                },
            )
            .await
            .is_none(),
            "a late completion must not refresh a replacement stable id"
        );
    }

    #[tokio::test]
    async fn late_pending_hydration_after_account_removal_caches_nothing_and_emits_nothing() {
        let store = Store::in_memory().await.unwrap();
        let account = dakia_core::AccountDraft {
            email: "late-hydration@example.test".into(),
            display_name: "Late hydration".into(),
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
        .into_account(dakia_core::provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();

        let fetched = save_hydration_target(&store, &account, "removed-while-fetching", 77).await;
        let mut fetched = fetched;
        fetched.body_text = "late body".into();
        fetched.content_state = "complete".into();
        let target = HydrationTarget {
            kind: HydrationKind::Pending,
            message: fetched.clone(),
        };
        let cached = cache_hydrated_message(&store, &target.message, fetched)
            .await
            .unwrap();
        assert!(
            cached.is_some(),
            "the race must reach event publication with a completed cache"
        );
        store.delete_account(account.id).await.unwrap();
        assert!(store.account(account.id).await.unwrap().is_none());
        assert!(store
            .message("removed-while-fetching")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .cached_message_content("removed-while-fetching")
            .await
            .unwrap()
            .is_none());
        assert!(pending_hydration_event(
            &store,
            account.id,
            &HydrationResult {
                target,
                started: Instant::now(),
                result: Ok(cached),
                cancelled: false,
            },
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn failed_pending_hydration_never_creates_a_mail_hydrated_event() {
        let store = Store::in_memory().await.unwrap();
        let target = HydrationTarget {
            kind: HydrationKind::Pending,
            message: message("failed-hydration"),
        };
        let outcome = HydrationResult {
            target,
            started: Instant::now(),
            result: Err(anyhow::anyhow!("message body download failed")),
            cancelled: false,
        };

        assert!(
            pending_hydration_event(&store, Uuid::new_v4(), &outcome)
                .await
                .is_none(),
            "the event sink must only receive mail-hydrated after a durable successful hydration"
        );
    }

    #[test]
    fn successful_native_hydration_invokes_the_registered_hook() {
        let hooks: HydrationCompleteHookStore = Arc::new(StdRwLock::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        *hooks.write().unwrap() = Some(Arc::new({
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        }));

        notify_hydration_complete(&hooks);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stalled_warm_batch_does_not_block_the_next_idle_arrival() {
        let (arrival_sender, arrival_receiver) = mpsc::unbounded_channel();
        let (maintenance_sender, maintenance_receiver) = mpsc::channel(1);
        let (_cancel, cancel_receiver) = watch::channel(false);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let processed = Arc::new(Mutex::new(Vec::new()));
        let worker = tokio::spawn(run_prioritized_hydration_batches(
            arrival_receiver,
            maintenance_receiver,
            cancel_receiver,
            {
                let started = started.clone();
                let release = release.clone();
                let processed = processed.clone();
                move |batch: Vec<u32>, _| {
                    let started = started.clone();
                    let release = release.clone();
                    let processed = processed.clone();
                    async move {
                        if batch == [99] {
                            started.notify_one();
                            release.notified().await;
                        }
                        processed.lock().await.extend(batch);
                    }
                }
            },
        ));

        maintenance_sender.send(vec![99]).await.unwrap();
        started.notified().await;
        // This is the next ~5-second IDLE cycle. Unlike the former bounded
        // combined queue, sending its arrival cannot await the stalled warm.
        arrival_sender
            .send(vec![1])
            .expect("the next arrival must queue without waiting for warming");
        maintenance_sender
            .try_send(vec![100])
            .expect("one maintenance request may wait behind the active warm");
        assert!(maintenance_sender.try_send(vec![101]).is_err());
        drop(arrival_sender);
        drop(maintenance_sender);
        release.notify_one();
        worker.await.unwrap();

        assert_eq!(*processed.lock().await, vec![99, 1, 100]);
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

    #[tokio::test]
    async fn concurrent_account_replacements_leave_one_registered_live_watcher() {
        let account_id = Uuid::new_v4();
        let account_task_locks = Arc::new(AccountOperationLocks::default());
        let tasks = Arc::new(Mutex::new(HashMap::new()));
        let running = Arc::new(AtomicUsize::new(0));
        let start_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let (entered_transition, mut entered_transition_receiver) = mpsc::channel(2);
        let (release_replacement, release_receiver) = watch::channel(false);

        let first = tokio::spawn(replace_test_watcher(
            account_task_locks.clone(),
            tasks.clone(),
            account_id,
            start_barrier.clone(),
            entered_transition.clone(),
            release_receiver.clone(),
            running.clone(),
        ));
        let second = tokio::spawn(replace_test_watcher(
            account_task_locks.clone(),
            tasks.clone(),
            account_id,
            start_barrier,
            entered_transition,
            release_receiver,
            running.clone(),
        ));

        // Both replacements attempt to start together. The first owns the
        // account transition while paused before its spawn/insert, so the
        // second cannot reach that critical section and overwrite its handle.
        entered_transition_receiver.recv().await.unwrap();
        tokio::task::yield_now().await;
        assert!(entered_transition_receiver.try_recv().is_err());

        release_replacement.send(true).unwrap();
        first.await.unwrap();
        second.await.unwrap();

        assert_eq!(running.load(Ordering::SeqCst), 1);
        assert_eq!(tasks.lock().await.len(), 1);

        stop_watcher_task(tasks.lock().await.remove(&account_id)).await;
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn thousand_unique_account_watcher_lifecycles_release_tasks_and_transition_locks() {
        const ACCOUNT_CYCLES: usize = 1_000;
        let account_task_locks = AccountOperationLocks::default();
        let tasks = Mutex::new(HashMap::new());
        let running = Arc::new(AtomicUsize::new(0));

        for _ in 0..ACCOUNT_CYCLES {
            let account_id = Uuid::new_v4();
            {
                let _transition = account_task_lock(&account_task_locks, account_id).await;
                replace_watcher_task(&tasks, account_id, async {
                    tracked_watcher(Arc::clone(&running))
                })
                .await;
            }
            {
                let _transition = account_task_lock(&account_task_locks, account_id).await;
                stop_watcher_task(tasks.lock().await.remove(&account_id)).await;
            }
            assert_eq!(running.load(Ordering::SeqCst), 0);
            assert!(tasks.lock().await.is_empty());
        }

        assert_eq!(account_task_locks.retained_entries(), 0);
    }

    #[tokio::test]
    async fn repeated_manager_stop_and_account_removal_release_connections_without_late_events() {
        const ACCOUNT_CYCLES: usize = 128;
        let store = Store::in_memory().await.expect("open lifecycle store");
        let manager = RealtimeSyncManager::new(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind lifecycle listener");
        let listener_address = listener.local_addr().expect("lifecycle listener address");
        let running = Arc::new(AtomicUsize::new(0));
        let published = Arc::new(AtomicUsize::new(0));

        for cycle in 0..ACCOUNT_CYCLES {
            let mut account = realtime_account();
            account.email = format!("lifecycle-{cycle}@example.test");
            store
                .save_account(&account)
                .await
                .expect("persist lifecycle account");

            let (client, accepted) = tokio::join!(
                tokio::net::TcpStream::connect(listener_address),
                listener.accept()
            );
            let client = client.expect("connect lifecycle watcher socket");
            let (mut server, _) = accepted.expect("accept lifecycle watcher socket");
            let (cancel, mut receiver) = watch::channel(false);
            let watcher_running = Arc::clone(&running);
            let watcher_published = Arc::clone(&published);
            watcher_running.fetch_add(1, Ordering::SeqCst);
            let handle = tauri::async_runtime::spawn(async move {
                // This models a watcher that has reached its connected event
                // loop. The manager must signal it, await it, and only then
                // return from stop_account.
                watcher_published.fetch_add(1, Ordering::SeqCst);
                wait_for_cancellation(&mut receiver).await;
                drop(client);
                watcher_running.fetch_sub(1, Ordering::SeqCst);
            });

            {
                let _transition = account_task_lock(&manager.account_task_locks, account.id).await;
                replace_watcher_task(&manager.tasks, account.id, async {
                    WatcherTask { cancel, handle }
                })
                .await;
            }
            manager.statuses.write().await.insert(
                account.id,
                RealtimeSyncStatus {
                    account_id: account.id,
                    state: "idle".into(),
                    retry_at: None,
                    error_kind: None,
                },
            );

            tokio::time::timeout(Duration::from_secs(1), manager.stop_account(account.id))
                .await
                .expect("manager stop must await the connected watcher");
            assert_eq!(
                running.load(Ordering::SeqCst),
                0,
                "stop_account must not return while its watcher is still alive"
            );
            let mut trailing = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut trailing))
                .await
                .expect("stopped watcher must close its provider socket")
                .expect("read lifecycle socket EOF");

            store
                .delete_account_with_credential(account.id)
                .await
                .expect("remove lifecycle account and credential atomically");
            let events_after_stop = published.load(Ordering::SeqCst);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }

            assert_eq!(published.load(Ordering::SeqCst), events_after_stop);
            assert_eq!(running.load(Ordering::SeqCst), 0);
            assert!(manager.tasks.lock().await.is_empty());
            assert!(manager.statuses().await.is_empty());
            assert_eq!(manager.account_task_locks.retained_entries(), 0);
            assert_eq!(
                manager.hydration_slots.available_permits(),
                HYDRATION_CONCURRENCY
            );
            assert!(store
                .account(account.id)
                .await
                .expect("read removed lifecycle account")
                .is_none());
        }

        assert_eq!(published.load(Ordering::SeqCst), ACCOUNT_CYCLES);
        assert!(store
            .accounts()
            .await
            .expect("list lifecycle accounts")
            .is_empty());
    }
}
