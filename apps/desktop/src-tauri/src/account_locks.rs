use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

type AccountLockRegistry = Arc<Mutex<HashMap<Uuid, AccountLockEntry>>>;

struct AccountLockEntry {
    lock: Arc<AsyncMutex<()>>,
    users: usize,
}

/// Serializes work for one account while allowing unrelated accounts to make
/// progress independently. A registry entry exists only while a lock owner or
/// waiter holds a lease, including while an acquisition future is pending.
#[derive(Default)]
pub(crate) struct AccountOperationLocks {
    locks: AccountLockRegistry,
}

struct AccountLockLease {
    account_id: Uuid,
    lock: Arc<AsyncMutex<()>>,
    registry: AccountLockRegistry,
}

pub(crate) struct AccountOperationGuard {
    guard: Option<OwnedMutexGuard<()>>,
    lease: Option<AccountLockLease>,
}

impl AccountOperationLocks {
    pub(crate) async fn acquire(&self, account_id: Uuid) -> AccountOperationGuard {
        let lease = {
            let mut locks = self
                .locks
                .lock()
                .expect("account operation lock registry poisoned");
            let entry = locks.entry(account_id).or_insert_with(|| AccountLockEntry {
                lock: Arc::new(AsyncMutex::new(())),
                users: 0,
            });
            entry.users += 1;
            AccountLockLease {
                account_id,
                lock: Arc::clone(&entry.lock),
                registry: Arc::clone(&self.locks),
            }
        };
        let guard = Arc::clone(&lease.lock).lock_owned().await;
        AccountOperationGuard {
            guard: Some(guard),
            lease: Some(lease),
        }
    }

    #[cfg(test)]
    pub(crate) fn retained_entries(&self) -> usize {
        self.locks
            .lock()
            .expect("account operation lock registry poisoned")
            .len()
    }

    #[cfg(test)]
    pub(crate) fn retained_users(&self, account_id: Uuid) -> usize {
        self.locks
            .lock()
            .expect("account operation lock registry poisoned")
            .get(&account_id)
            .map_or(0, |entry| entry.users)
    }
}

impl Drop for AccountOperationGuard {
    fn drop(&mut self) {
        // Keep the registry entry until the Tokio guard is released. Removing
        // the lease first could let a new caller create a second mutex for the
        // same account while this guard still owns the original mutex.
        drop(self.guard.take());
        drop(self.lease.take());
    }
}

impl Drop for AccountLockLease {
    fn drop(&mut self) {
        let mut locks = self
            .registry
            .lock()
            .expect("account operation lock registry poisoned");
        let should_remove = match locks.get_mut(&self.account_id) {
            Some(entry) if Arc::ptr_eq(&entry.lock, &self.lock) => {
                entry.users = entry
                    .users
                    .checked_sub(1)
                    .expect("account operation lock lease underflow");
                entry.users == 0
            }
            _ => false,
        };
        if should_remove {
            locks.remove(&self.account_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_account_waiters_share_one_lock_and_cleanup_after_the_last_guard() {
        let locks = Arc::new(AccountOperationLocks::default());
        let account_id = Uuid::new_v4();
        let first = locks.acquire(account_id).await;
        let waiting_locks = Arc::clone(&locks);
        let waiter = tokio::spawn(async move { waiting_locks.acquire(account_id).await });

        tokio::task::yield_now().await;
        assert_eq!(locks.retained_entries(), 1);
        drop(first);
        let second = waiter.await.expect("waiter should acquire account lock");
        assert_eq!(locks.retained_entries(), 1);
        drop(second);
        assert_eq!(locks.retained_entries(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_its_registry_lease() {
        let locks = Arc::new(AccountOperationLocks::default());
        let account_id = Uuid::new_v4();
        let first = locks.acquire(account_id).await;
        let waiting_locks = Arc::clone(&locks);
        let waiter = tokio::spawn(async move { waiting_locks.acquire(account_id).await });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while locks.retained_users(account_id) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter should register its cancellable lease");
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert_eq!(locks.retained_entries(), 1);
        drop(first);
        assert_eq!(locks.retained_entries(), 0);
    }

    #[tokio::test]
    async fn one_thousand_concurrent_unique_accounts_cleanup_after_a_burst() {
        let locks = AccountOperationLocks::default();
        let mut guards = Vec::with_capacity(1_000);
        for _ in 0..1_000 {
            guards.push(locks.acquire(Uuid::new_v4()).await);
        }
        assert_eq!(locks.retained_entries(), 1_000);

        drop(guards);
        assert_eq!(locks.retained_entries(), 0);
    }

    #[tokio::test]
    async fn unrelated_account_acquires_while_another_account_is_held() {
        let locks = AccountOperationLocks::default();
        let first = locks.acquire(Uuid::new_v4()).await;
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            locks.acquire(Uuid::new_v4()),
        )
        .await
        .expect("unrelated account must not wait for the held account");

        drop(second);
        drop(first);
        assert_eq!(locks.retained_entries(), 0);
    }
}
