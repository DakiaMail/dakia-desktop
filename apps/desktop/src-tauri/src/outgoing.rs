use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use dakia_core::Store;

/// Renews a journal claim while the caller is inside a remote SMTP or IMAP
/// command. Recovery only takes expired claims, so a second process never
/// converts a live provider attempt into an ambiguous result.
pub struct ClaimHeartbeat {
    stop: tokio::sync::watch::Sender<bool>,
}

impl ClaimHeartbeat {
    pub fn start(store: Store, operation_id: String, claim_owner: String) -> Self {
        let (stop, mut receiver) = tokio::sync::watch::channel(false);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::select! {
                    changed = receiver.changed() => {
                        if changed.is_err() || *receiver.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        match store.renew_operation_claim(&operation_id, &claim_owner).await {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => tracing::warn!(operation_id = %operation_id, error = %error, "could not renew operation claim"),
                        }
                    }
                }
            }
        });
        Self { stop }
    }
}

impl Drop for ClaimHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// Keeps SMTP submission independent from long-running IMAP work while still
/// preserving SMTP's required per-account order. Account removal first blocks
/// new submissions and then waits briefly for the active one to reach a safe
/// boundary.
#[derive(Default)]
pub struct SubmissionCoordinator {
    entries: Mutex<HashMap<Uuid, SubmissionEntry>>,
}

pub struct SubmissionPause {
    coordinator: Arc<SubmissionCoordinator>,
    account_id: Uuid,
}

struct SubmissionEntry {
    lock: Arc<AsyncMutex<()>>,
    blocked: bool,
}

impl SubmissionCoordinator {
    pub async fn acquire(
        &self,
        account_id: Uuid,
    ) -> Result<OwnedMutexGuard<()>, SubmissionBlocked> {
        let lock = {
            let mut entries = self
                .entries
                .lock()
                .expect("submission coordinator lock poisoned");
            let entry = entries
                .entry(account_id)
                .or_insert_with(|| SubmissionEntry {
                    lock: Arc::new(AsyncMutex::new(())),
                    blocked: false,
                });
            if entry.blocked {
                return Err(SubmissionBlocked);
            }
            entry.lock.clone()
        };
        let guard = lock.lock_owned().await;
        if self.is_blocked(account_id) {
            drop(guard);
            return Err(SubmissionBlocked);
        }
        Ok(guard)
    }

    /// Prevents newly queued submissions and waits at most `timeout` for the
    /// current submission. The caller owns account deletion after this method
    /// returns, whether or not the bounded wait elapsed.
    pub async fn block_and_wait(&self, account_id: Uuid, timeout: Duration) -> bool {
        let lock = {
            let mut entries = self
                .entries
                .lock()
                .expect("submission coordinator lock poisoned");
            let entry = entries
                .entry(account_id)
                .or_insert_with(|| SubmissionEntry {
                    lock: Arc::new(AsyncMutex::new(())),
                    blocked: true,
                });
            entry.blocked = true;
            entry.lock.clone()
        };
        match tokio::time::timeout(timeout, lock.lock_owned()).await {
            Ok(guard) => {
                drop(guard);
                true
            }
            Err(_) => false,
        }
    }

    /// Releases a removal gate after the removal was abandoned. Existing
    /// submissions remain serialized by the same per-account lock.
    pub fn unblock(&self, account_id: Uuid) {
        if let Some(entry) = self
            .entries
            .lock()
            .expect("submission coordinator lock poisoned")
            .get_mut(&account_id)
        {
            entry.blocked = false;
        }
    }

    /// Briefly blocks new SMTP submissions while account credentials or SMTP
    /// settings are replaced. Dropping the returned guard always reopens the
    /// submission gate, including every early-return settings validation path.
    pub async fn pause(
        self: &Arc<Self>,
        account_id: Uuid,
        timeout: Duration,
    ) -> Result<SubmissionPause, SubmissionBlocked> {
        if !self.block_and_wait(account_id, timeout).await {
            self.unblock(account_id);
            return Err(SubmissionBlocked);
        }
        Ok(SubmissionPause {
            coordinator: self.clone(),
            account_id,
        })
    }

    fn is_blocked(&self, account_id: Uuid) -> bool {
        self.entries
            .lock()
            .expect("submission coordinator lock poisoned")
            .get(&account_id)
            .is_some_and(|entry| entry.blocked)
    }
}

impl Drop for SubmissionPause {
    fn drop(&mut self) {
        self.coordinator.unblock(self.account_id);
    }
}

#[derive(Debug)]
pub struct SubmissionBlocked;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serializes_only_submissions_for_the_same_account() {
        let coordinator = Arc::new(SubmissionCoordinator::default());
        let account_id = Uuid::new_v4();
        let first = coordinator.acquire(account_id).await.unwrap();
        let waiting = coordinator.clone();
        let task = tokio::spawn(async move { waiting.acquire(account_id).await.is_ok() });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(first);
        assert!(task.await.unwrap());
    }

    #[tokio::test]
    async fn block_rejects_new_submissions_and_has_a_bounded_wait() {
        let coordinator = SubmissionCoordinator::default();
        let account_id = Uuid::new_v4();
        let active = coordinator.acquire(account_id).await.unwrap();
        assert!(
            !coordinator
                .block_and_wait(account_id, Duration::from_millis(1))
                .await
        );
        assert!(coordinator.acquire(account_id).await.is_err());
        drop(active);
        coordinator.unblock(account_id);
        assert!(coordinator.acquire(account_id).await.is_ok());
    }
}
