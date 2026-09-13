//! One process-wide IMAP budget, shared by independently created services.
//! Background work cannot consume the slots reserved for user interactions.
//! SMTP deliberately does not participate in this budget.

use std::{
    future::Future,
    sync::{Arc, OnceLock},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const CONNECTIONS: usize = 8;
const BACKGROUND_CONNECTIONS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImapPriority {
    Foreground,
    Realtime,
    FolderRefresh,
    History,
    Deferred,
    Preview,
}

tokio::task_local! { static PRIORITY: ImapPriority; }

struct Budget {
    all: Arc<Semaphore>,
    background: Arc<Semaphore>,
    lower_lanes: Vec<Arc<Semaphore>>,
}

impl Budget {
    fn new(all: usize, background: usize) -> Self {
        assert!(background > 0 && background < all);
        Self {
            all: Arc::new(Semaphore::new(all)),
            background: Arc::new(Semaphore::new(background)),
            lower_lanes: (1..background)
                .rev()
                .map(|slots| Arc::new(Semaphore::new(slots)))
                .collect(),
        }
    }

    async fn acquire(&self, priority: ImapPriority) -> ImapConnectionPermit {
        let lane_count = match priority {
            ImapPriority::Foreground | ImapPriority::Realtime => 0,
            ImapPriority::FolderRefresh => 1,
            ImapPriority::History => 2,
            ImapPriority::Deferred => 3,
            ImapPriority::Preview => 4,
        }
        .min(self.lower_lanes.len());
        let mut lower_lanes = Vec::with_capacity(lane_count);
        // Take the narrowest allowance first. Waiting low-priority work must
        // never reserve a slot needed by higher-priority work.
        for lane in self.lower_lanes[..lane_count].iter().rev() {
            lower_lanes.push(
                lane.clone()
                    .acquire_owned()
                    .await
                    .expect("IMAP budget stays open"),
            );
        }
        // Acquire the background allowance first. A waiting maintenance job
        // must never reserve a foreground slot while its own allowance is full.
        let background = if priority != ImapPriority::Foreground {
            Some(
                self.background
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("IMAP background budget stays open"),
            )
        } else {
            None
        };
        let all = self
            .all
            .clone()
            .acquire_owned()
            .await
            .expect("IMAP connection budget stays open");
        ImapConnectionPermit {
            _all: all,
            _background: background,
            _lower_lanes: lower_lanes,
        }
    }
}

pub struct ImapConnectionPermit {
    _all: OwnedSemaphorePermit,
    _background: Option<OwnedSemaphorePermit>,
    _lower_lanes: Vec<OwnedSemaphorePermit>,
}

/// Retain this permit with the socket until EOF, cancellation, or drop. Never
/// return the slot at command completion while the underlying socket is alive.
pub async fn acquire_imap_connection() -> ImapConnectionPermit {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET
        .get_or_init(|| Budget::new(CONNECTIONS, BACKGROUND_CONNECTIONS))
        .acquire(
            PRIORITY
                .try_with(|priority| *priority)
                .unwrap_or(ImapPriority::Foreground),
        )
        .await
}

/// Scope automatic history, IDLE and content warming as background work.
/// Spawned tasks must enter their own scope because Tokio task locals are not
/// inherited when a new task is spawned.
pub async fn background_imap<T>(future: impl Future<Output = T>) -> T {
    imap_work(ImapPriority::History, future).await
}

pub async fn imap_work<T>(priority: ImapPriority, future: impl Future<Output = T>) -> T {
    PRIORITY.scope(priority, future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stalled_background_connections_leave_a_foreground_slot() {
        let budget = Arc::new(Budget::new(3, 2));
        let first = budget.acquire(ImapPriority::Realtime).await;
        let second = budget.acquire(ImapPriority::Realtime).await;
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { budget.acquire(ImapPriority::Realtime).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        let foreground = tokio::time::timeout(
            Duration::from_millis(100),
            budget.acquire(ImapPriority::Foreground),
        )
        .await
        .expect("reading must remain available during stalled history");
        assert_eq!(budget.all.available_permits(), 0);
        drop(first);
        let third = waiting.await.unwrap();
        drop((second, foreground, third));
        assert_eq!(budget.all.available_permits(), 3);
        assert_eq!(budget.background.available_permits(), 2);
    }

    #[tokio::test]
    async fn cancellation_returns_background_allowance_while_waiting_for_socket() {
        let budget = Arc::new(Budget::new(2, 1));
        let first = budget.acquire(ImapPriority::Foreground).await;
        let second = budget.acquire(ImapPriority::Foreground).await;
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { budget.acquire(ImapPriority::Realtime).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(budget.background.available_permits(), 0);
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(budget.background.available_permits(), 1);
        drop((first, second));
        assert_eq!(budget.all.available_permits(), 2);
    }

    #[tokio::test]
    async fn stalled_low_priority_work_leaves_capacity_for_each_higher_lane() {
        let budget = Budget::new(CONNECTIONS, BACKGROUND_CONNECTIONS);
        let mut held = Vec::new();
        for priority in [
            ImapPriority::Preview,
            ImapPriority::Deferred,
            ImapPriority::History,
            ImapPriority::FolderRefresh,
            ImapPriority::Realtime,
            ImapPriority::Foreground,
            ImapPriority::Foreground,
            ImapPriority::Foreground,
        ] {
            held.push(
                tokio::time::timeout(Duration::from_millis(100), budget.acquire(priority))
                    .await
                    .expect("lower-priority stalled work must leave higher-priority work usable"),
            );
        }
        assert_eq!(budget.all.available_permits(), 0);
        drop(held);
        assert_eq!(budget.all.available_permits(), CONNECTIONS);
        assert_eq!(
            budget.background.available_permits(),
            BACKGROUND_CONNECTIONS
        );
        assert_eq!(
            budget
                .lower_lanes
                .iter()
                .map(|lane| lane.available_permits())
                .collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
    }
}
