use dakia_core::{Account, CachedMessageContent, MailService, MailSummary, RealtimeMode, Store};
use serde::Serialize;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, watch, Mutex, OwnedMutexGuard, RwLock, Semaphore};
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
    account_task_locks: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
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
            account_task_locks: Arc::new(Mutex::new(HashMap::new())),
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
    account_task_locks: &Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    account_id: Uuid,
) -> OwnedMutexGuard<()> {
    let lock = account_task_locks
        .lock()
        .await
        .entry(account_id)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    lock.lock_owned().await
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
    mut cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
) {
    let mut failures = 0_usize;
    // Arrivals must never wait behind a long cache-warm pass. Keep their queue
    // separate and prioritize it in the worker; maintenance is coalesced to a
    // single pending pass because an extra empty IDLE renewal has no payload.
    let (arrival_sender, arrival_receiver) = mpsc::unbounded_channel::<HydrationBatch>();
    let (maintenance_sender, maintenance_receiver) =
        mpsc::channel::<HydrationBatch>(HYDRATION_MAINTENANCE_QUEUE_CAPACITY);
    let (hydration_cancel, hydration_cancel_receiver) = watch::channel(false);
    let hydration_handle = tauri::async_runtime::spawn(run_hydration_worker(
        app.clone(),
        store.clone(),
        account.clone(),
        HydrationWorkerQueues {
            arrivals: arrival_receiver,
            maintenance: maintenance_receiver,
        },
        hydration_cancel_receiver,
        hydration_slots,
        hydration_complete,
    ));
    let mut last_body_cache_warm = None;
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
                // Only a real arrival enters the unbounded priority queue. A
                // non-warm empty renewal is intentionally dropped, and a warm
                // request supersedes an already queued maintenance request.
                let now = Instant::now();
                let warm_recent = body_cache_warm_due(last_body_cache_warm, now);
                if warm_recent {
                    last_body_cache_warm = Some(now);
                }
                if !cycle.pending_hydration.is_empty()
                    && arrival_sender
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
                if warm_recent {
                    // Full means a maintenance pass is already queued. It is
                    // equivalent to this one, so coalescing preserves the
                    // cadence without letting the IDLE watcher block.
                    let _ = maintenance_sender.try_send(HydrationBatch {
                        pending: Vec::new(),
                        warm_recent: true,
                    });
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
    drop(arrival_sender);
    drop(maintenance_sender);
    let _ = hydration_handle.await;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentCacheDestination {
    Regular,
    Starred,
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
    let durable = store.message(&hydrated.id).await.ok().flatten()?;
    if durable.account_id != account_id.to_string() {
        return None;
    }
    Some(MailHydrated {
        account_id,
        message_id: hydrated.id.clone(),
    })
}

#[derive(Clone)]
struct HydrationContext {
    app: AppHandle,
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

fn content_cache_destination(message: &MailSummary) -> ContentCacheDestination {
    if message.is_flagged {
        ContentCacheDestination::Starred
    } else {
        ContentCacheDestination::Regular
    }
}

fn display_cache_content(message: &MailSummary) -> CachedMessageContent {
    CachedMessageContent {
        body_text: message.body_text.clone(),
        body_html: message.body_html.clone(),
        unsubscribe_kind: message.unsubscribe_kind.clone(),
        attachments: message
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.clone())
            .collect(),
    }
}

async fn run_hydration_worker(
    app: AppHandle,
    store: Store,
    account: Account,
    queues: HydrationWorkerQueues,
    cancel: watch::Receiver<bool>,
    hydration_slots: Arc<Semaphore>,
    hydration_complete: HydrationCompleteHookStore,
) {
    let service = MailService::new(store.clone());
    let context = HydrationContext {
        app,
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
                let _ = context.app.emit(
                    "mail-changed",
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
            let _ = context.app.emit("mail-hydrated", event);
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
    cache_hydrated_message(store, message).await
}

async fn cache_hydrated_message(
    store: &Store,
    message: MailSummary,
) -> anyhow::Result<Option<MailSummary>> {
    if persist_display_cache(store, &message).await? {
        // Cache readiness is local state. Do not upsert the provider snapshot
        // here: a star/read operation may have finished while this body fetch
        // was in flight, and stale FLAGS must not undo that user action.
        store
            .set_message_content_state(&message.id, "complete")
            .await?;
        // Account removal can complete after the cache write but before event
        // publication. Re-read the durable row so a late worker neither
        // revives storage nor reports a hydration for a removed account.
        Ok(store.message(&message.id).await?.map(|_| message))
    } else {
        Ok(None)
    }
}

async fn persist_display_cache(store: &Store, message: &MailSummary) -> anyhow::Result<bool> {
    if !store
        .update_message_attachment_state(&message.id, message.has_attachments)
        .await?
    {
        return Ok(false);
    }
    let content = display_cache_content(message);

    if content_cache_destination(message) == ContentCacheDestination::Starred
        && store
            .cache_starred_message_content(&message.id, content.clone())
            .await?
    {
        return Ok(true);
    }

    // A concurrent unstar makes the regular cache authoritative. Conversely,
    // if a message was starred while this selective fetch was in flight, the
    // regular-cache write declines and the final starred attempt owns it.
    store
        .cache_message_content(&message.id, false, content.clone())
        .await?;
    if store.cached_message_content(&message.id).await?.is_some() {
        return Ok(true);
    }
    store
        .cache_starred_message_content(&message.id, content)
        .await
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
        account_task_locks: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
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

    #[test]
    fn display_cache_decision_uses_the_authoritative_flagged_cache() {
        let regular = message("regular");
        assert_eq!(
            content_cache_destination(&regular),
            ContentCacheDestination::Regular
        );
        let mut starred = regular.clone();
        starred.is_flagged = true;
        starred.body_text = "display body".into();
        starred.body_html = Some("<p>display body</p>".into());
        starred.unsubscribe_kind = Some("mailto".into());
        assert_eq!(
            content_cache_destination(&starred),
            ContentCacheDestination::Starred
        );
        let content = display_cache_content(&starred);
        assert_eq!(content.body_text, "display body");
        assert_eq!(content.body_html.as_deref(), Some("<p>display body</p>"));
        assert_eq!(content.unsubscribe_kind.as_deref(), Some("mailto"));
    }

    #[tokio::test]
    async fn background_cache_commit_preserves_concurrent_star_changes() {
        let store = Store::in_memory().await.unwrap();
        let local = message("star-race");
        store
            .upsert_messages(std::slice::from_ref(&local))
            .await
            .unwrap();

        let mut stale_starred = local.clone();
        stale_starred.is_flagged = true;
        stale_starred.body_text = "fetched while starred".into();
        assert!(persist_display_cache(&store, &stale_starred).await.unwrap());
        assert!(!store.message(&local.id).await.unwrap().unwrap().is_flagged);
        assert!(store
            .cached_message_content(&local.id)
            .await
            .unwrap()
            .is_some());

        store.set_message_flagged(&local.id, true).await.unwrap();
        let mut stale_unstarred = local.clone();
        stale_unstarred.body_text = "fetched before star".into();
        assert!(persist_display_cache(&store, &stale_unstarred)
            .await
            .unwrap());
        assert!(store.message(&local.id).await.unwrap().unwrap().is_flagged);
        assert!(store.starred_body(&local.id).await.unwrap().is_some());
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

        let mut fetched = message("removed-while-fetching");
        fetched.account_id = account.id.to_string();
        fetched.body_text = "late body".into();
        let target = HydrationTarget {
            kind: HydrationKind::Pending,
            message: fetched.clone(),
        };
        store
            .upsert_messages(std::slice::from_ref(&fetched))
            .await
            .unwrap();

        let cached = cache_hydrated_message(&store, fetched).await.unwrap();
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
        let account_task_locks = Arc::new(Mutex::new(HashMap::new()));
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
}
