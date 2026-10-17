//! Cross-process ownership for short account-scoped provider operations.
//!
//! A lease is deliberately **not** a mutex for an entire mailbox sync.  A
//! history sync spends most of its time downloading data and must leave other
//! account actions responsive.  Acquire [`AccountOperationKind::SyncPublish`]
//! only for the short transition that publishes a staged sync result.  Acquire
//! [`AccountOperationKind::ProviderMutation`] around one remote mutation and
//! its local reconciliation.  The guard renews while it is alive and is safe
//! to abandon: another process may recover it after the bounded lease expires.

use crate::{AccountId, Store};
use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::time::Duration;

/// The normal lease window for an account operation.
///
/// It is longer than one provider command timeout, while the background
/// heartbeat keeps a live command from becoming available to another process.
pub const ACCOUNT_OPERATION_LEASE_DURATION: Duration = Duration::from_secs(90);
/// A sync publication may briefly wait for an in-flight user action, but it
/// must not turn a historical sync into an unbounded account-wide queue.
const SYNC_PUBLISH_ACQUIRE_WAIT: Duration = Duration::from_secs(2);
const SYNC_PUBLISH_RETRY_DELAY: Duration = Duration::from_millis(50);

/// The class of short account operation that currently owns the account.
///
/// Classes share one account-scoped lease.  They are recorded to make an
/// unavailable action explainable; they do not create independent locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountOperationKind {
    /// One provider-side change such as flagging, moving, deleting, or
    /// sending, together with its local reconciliation.
    ProviderMutation,
    /// The local transaction that publishes a staged sync result.  Do not hold
    /// this while fetching historical pages from the provider.
    SyncPublish,
    /// Account deletion.  Hold this through `Store::delete_account` so a UI
    /// can present a deterministic busy result instead of racing a mutation.
    AccountDeletion,
}

impl AccountOperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderMutation => "provider_mutation",
            Self::SyncPublish => "sync_publish",
            Self::AccountDeletion => "account_deletion",
        }
    }
}

/// Why an account operation could not acquire ownership.
pub enum AccountOperationAcquire {
    /// The caller owns the operation until the returned guard is released or
    /// dropped.
    Acquired(AccountOperationLease),
    /// Another live process owns a short operation for this account.
    Busy,
    /// The account has been removed.  Never retry provider work for it.
    AccountDeleted,
    /// The account no longer exists, but has no durable deletion tombstone.
    AccountMissing,
}

/// Whether a previously acquired guard may still publish work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountOperationLeaseState {
    Active,
    Lost,
    AccountDeleted,
    AccountMissing,
}

/// A coordinator backed by the profile's SQLite database.
///
/// It is cheap to clone.  A profile can have desktop and CLI coordinators at
/// the same time because ownership is stored in SQLite, rather than memory.
#[derive(Clone)]
pub struct AccountOperationCoordinator {
    pool: SqlitePool,
    lease_duration: Duration,
}

/// Cancellation-safe ownership of one short account operation.
///
/// Dropping the guard asks SQLite to release the exact owner token.  If the
/// runtime has already stopped, the durable expiry still recovers the lease.
pub struct AccountOperationLease {
    coordinator: AccountOperationCoordinator,
    account_id: String,
    owner_token: String,
    renewal: tokio::task::JoinHandle<()>,
    released: bool,
}

impl Store {
    /// Returns a cross-process coordinator for this profile.
    ///
    /// This is intentionally a separate opt-in from opening `Store`: callers
    /// that only read mail do not need an operation lease table.
    pub async fn operation_coordinator(&self) -> Result<AccountOperationCoordinator> {
        AccountOperationCoordinator::new(self.pool.clone(), ACCOUNT_OPERATION_LEASE_DURATION).await
    }

    /// Acquires the short local-publication phase of a sync.
    ///
    /// The bounded retry lets a sync finish its already-fetched publication
    /// immediately after a short provider mutation. Call this directly before
    /// a `BEGIN IMMEDIATE` publication transaction and release it immediately
    /// after commit. Never hold it while downloading mailbox history.
    pub async fn acquire_sync_publish(
        &self,
        account_id: AccountId,
    ) -> Result<AccountOperationLease> {
        let coordinator = self.operation_coordinator().await?;
        let deadline = tokio::time::Instant::now() + SYNC_PUBLISH_ACQUIRE_WAIT;
        loop {
            match coordinator
                .acquire(account_id, AccountOperationKind::SyncPublish)
                .await?
            {
                AccountOperationAcquire::Acquired(lease) => return Ok(lease),
                AccountOperationAcquire::AccountDeleted => {
                    return Err(anyhow!("account was removed"))
                }
                AccountOperationAcquire::AccountMissing => {
                    return Err(anyhow!("account does not exist"));
                }
                AccountOperationAcquire::Busy if tokio::time::Instant::now() >= deadline => {
                    return Err(anyhow!(
                        "another Dakia operation is active for this account; retry shortly"
                    ));
                }
                AccountOperationAcquire::Busy => {
                    tokio::time::sleep(SYNC_PUBLISH_RETRY_DELAY).await;
                }
            }
        }
    }
}

impl AccountOperationCoordinator {
    async fn new(pool: SqlitePool, lease_duration: Duration) -> Result<Self> {
        let coordinator = Self {
            pool,
            lease_duration,
        };
        coordinator.ensure_schema().await?;
        Ok(coordinator)
    }

    async fn ensure_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_operation_leases (account_id TEXT PRIMARY KEY, operation_kind TEXT NOT NULL, owner_token TEXT NOT NULL, expires_at_millis INTEGER NOT NULL, created_at_millis INTEGER NOT NULL, updated_at_millis INTEGER NOT NULL)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Tries to own one account operation without waiting behind long network
    /// work.  A live owner returns [`AccountOperationAcquire::Busy`].
    pub async fn acquire(
        &self,
        account_id: AccountId,
        kind: AccountOperationKind,
    ) -> Result<AccountOperationAcquire> {
        let account_id = account_id.to_string();
        let owner_token = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        let expires_at = now.saturating_add(duration_millis(self.lease_duration));
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let deleted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(&account_id)
        .fetch_one(&mut *transaction)
        .await?;
        if deleted {
            transaction.rollback().await?;
            return Ok(AccountOperationAcquire::AccountDeleted);
        }

        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
            .bind(&account_id)
            .fetch_one(&mut *transaction)
            .await?;
        if !exists {
            transaction.rollback().await?;
            return Ok(AccountOperationAcquire::AccountMissing);
        }

        let claimed = sqlx::query(
            "INSERT INTO account_operation_leases(account_id, operation_kind, owner_token, expires_at_millis, created_at_millis, updated_at_millis) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(account_id) DO UPDATE SET operation_kind = excluded.operation_kind, owner_token = excluded.owner_token, expires_at_millis = excluded.expires_at_millis, updated_at_millis = excluded.updated_at_millis WHERE account_operation_leases.expires_at_millis <= ?",
        )
        .bind(&account_id)
        .bind(kind.as_str())
        .bind(&owner_token)
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        transaction.commit().await?;

        if !claimed {
            return Ok(AccountOperationAcquire::Busy);
        }

        let renewal = self.start_renewal(account_id.clone(), owner_token.clone());
        Ok(AccountOperationAcquire::Acquired(AccountOperationLease {
            coordinator: self.clone(),
            account_id,
            owner_token,
            renewal,
            released: false,
        }))
    }

    /// Returns a guard or a descriptive error suitable for commands that
    /// cannot continue without ownership.
    pub async fn acquire_required(
        &self,
        account_id: AccountId,
        kind: AccountOperationKind,
    ) -> Result<AccountOperationLease> {
        match self.acquire(account_id, kind).await? {
            AccountOperationAcquire::Acquired(lease) => Ok(lease),
            AccountOperationAcquire::Busy => Err(anyhow!(
                "another Dakia operation is active for this account; retry shortly"
            )),
            AccountOperationAcquire::AccountDeleted => Err(anyhow!("account was removed")),
            AccountOperationAcquire::AccountMissing => Err(anyhow!("account does not exist")),
        }
    }

    fn start_renewal(
        &self,
        account_id: String,
        owner_token: String,
    ) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        tokio::spawn(async move {
            let interval = renewal_interval(coordinator.lease_duration);
            loop {
                tokio::time::sleep(interval).await;
                if !coordinator
                    .renew(&account_id, &owner_token)
                    .await
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
    }

    async fn renew(&self, account_id: &str, owner_token: &str) -> Result<bool> {
        let now = now_millis();
        let expires_at = now.saturating_add(duration_millis(self.lease_duration));
        let result = sqlx::query(
            "UPDATE account_operation_leases SET expires_at_millis = ?, updated_at_millis = ? WHERE account_id = ? AND owner_token = ? AND expires_at_millis > ? AND EXISTS(SELECT 1 FROM accounts WHERE id = ?) AND NOT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(expires_at)
        .bind(now)
        .bind(account_id)
        .bind(owner_token)
        .bind(now)
        .bind(account_id)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn release(&self, account_id: &str, owner_token: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM account_operation_leases WHERE account_id = ? AND owner_token = ?",
        )
        .bind(account_id)
        .bind(owner_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn state(
        &self,
        account_id: &str,
        owner_token: &str,
    ) -> Result<AccountOperationLeaseState> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let deleted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deleted_account_tombstones WHERE account_id = ?)",
        )
        .bind(account_id)
        .fetch_one(&mut *transaction)
        .await?;
        if deleted {
            transaction.rollback().await?;
            return Ok(AccountOperationLeaseState::AccountDeleted);
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?)")
            .bind(account_id)
            .fetch_one(&mut *transaction)
            .await?;
        if !exists {
            transaction.rollback().await?;
            return Ok(AccountOperationLeaseState::AccountMissing);
        }
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM account_operation_leases WHERE account_id = ? AND owner_token = ? AND expires_at_millis > ?)",
        )
        .bind(account_id)
        .bind(owner_token)
        .bind(now_millis())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(if active {
            AccountOperationLeaseState::Active
        } else {
            AccountOperationLeaseState::Lost
        })
    }
}

impl AccountOperationLease {
    /// Checks ownership immediately before publishing a remote result.
    ///
    /// Call this after a provider mutation and before its local write.  Storage
    /// tombstone triggers remain the final safety net if account deletion wins
    /// this race after the check.
    pub async fn state(&self) -> Result<AccountOperationLeaseState> {
        self.coordinator
            .state(&self.account_id, &self.owner_token)
            .await
    }

    /// Releases the exact owner token immediately.
    pub async fn release(mut self) -> Result<()> {
        self.renewal.abort();
        self.coordinator
            .release(&self.account_id, &self.owner_token)
            .await?;
        self.released = true;
        Ok(())
    }
}

impl Drop for AccountOperationLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.renewal.abort();
        let coordinator = self.coordinator.clone();
        let account_id = self.account_id.clone();
        let owner_token = self.owner_token.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = coordinator.release(&account_id, &owner_token).await;
            });
        }
    }
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn renewal_interval(lease_duration: Duration) -> Duration {
    // Keep renewal comfortably before expiry and avoid a busy loop for small
    // deterministic test leases.
    lease_duration
        .checked_div(3)
        .unwrap_or(Duration::from_millis(1))
        .max(Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider, Account, AccountDraft};
    use std::{path::Path, process::Stdio};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::{Child, Command},
        time::{sleep, timeout},
    };

    const READY: &str = "DAKIA_ACCOUNT_OPERATION_LEASE_READY";
    const TEST_LEASE_DURATION: Duration = Duration::from_millis(700);

    async fn coordinator_with_duration(
        store: &Store,
        lease_duration: Duration,
    ) -> AccountOperationCoordinator {
        AccountOperationCoordinator::new(store.pool.clone(), lease_duration)
            .await
            .expect("create operation coordinator")
    }

    async fn account(store: &Store, email: &str) -> Account {
        let account = AccountDraft {
            email: email.into(),
            display_name: "Operation test".into(),
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
        .into_account(provider::by_id("custom").expect("custom provider"));
        store.save_account(&account).await.expect("save account");
        account
    }

    async fn acquire(
        coordinator: &AccountOperationCoordinator,
        account: AccountId,
        kind: AccountOperationKind,
    ) -> AccountOperationLease {
        coordinator
            .acquire_required(account, kind)
            .await
            .expect("acquire account operation")
    }

    #[tokio::test]
    async fn account_operations_are_exclusive_per_account_but_independent_between_accounts() {
        let store = Store::in_memory().await.expect("open store");
        let first = account(&store, "first@example.test").await;
        let second = account(&store, "second@example.test").await;
        let coordinator = coordinator_with_duration(&store, TEST_LEASE_DURATION).await;
        let lease = acquire(
            &coordinator,
            first.id,
            AccountOperationKind::ProviderMutation,
        )
        .await;

        let rejected = coordinator
            .acquire(first.id, AccountOperationKind::SyncPublish)
            .await
            .expect("check competing account operation");
        assert!(matches!(rejected, AccountOperationAcquire::Busy));

        let other = acquire(
            &coordinator,
            second.id,
            AccountOperationKind::ProviderMutation,
        )
        .await;
        assert_eq!(
            lease.state().await.expect("lease state"),
            AccountOperationLeaseState::Active
        );
        other.release().await.expect("release second account");
        lease.release().await.expect("release first account");
    }

    #[tokio::test]
    async fn deletion_tombstone_rejects_late_operation_publication() {
        let store = Store::in_memory().await.expect("open store");
        let account = account(&store, "deleted@example.test").await;
        let coordinator = coordinator_with_duration(&store, TEST_LEASE_DURATION).await;
        let deletion = acquire(
            &coordinator,
            account.id,
            AccountOperationKind::AccountDeletion,
        )
        .await;

        store
            .delete_account(account.id)
            .await
            .expect("delete account");
        assert_eq!(
            deletion.state().await.expect("deletion guard state"),
            AccountOperationLeaseState::AccountDeleted
        );
        deletion.release().await.expect("release deletion guard");

        let outcome = coordinator
            .acquire(account.id, AccountOperationKind::ProviderMutation)
            .await
            .expect("check deleted account");
        assert!(matches!(outcome, AccountOperationAcquire::AccountDeleted));
    }

    #[tokio::test]
    async fn sync_publication_waits_briefly_for_a_provider_mutation() {
        let store = Store::in_memory().await.expect("open store");
        let account = account(&store, "sync-publication@example.test").await;
        let coordinator = coordinator_with_duration(&store, TEST_LEASE_DURATION).await;
        let mutation = acquire(
            &coordinator,
            account.id,
            AccountOperationKind::ProviderMutation,
        )
        .await;

        let waiting_store = store.clone();
        let waiter = tokio::spawn(async move {
            waiting_store
                .acquire_sync_publish(account.id)
                .await
                .expect("sync publication acquires after mutation")
        });
        sleep(Duration::from_millis(150)).await;
        mutation.release().await.expect("release provider mutation");
        let publication = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("bounded publication wait")
            .expect("publication task");
        publication
            .release()
            .await
            .expect("release sync publication");
    }

    async fn spawn_worker(database: &Path, account_id: AccountId) -> Child {
        let mut command = Command::new(std::env::current_exe().expect("test binary path"));
        command
            .args([
                "--exact",
                "operations::tests::cross_process_operation_lease_worker",
                "--nocapture",
            ])
            .env("DAKIA_OPERATION_LEASE_WORKER_DATABASE", database)
            .env(
                "DAKIA_OPERATION_LEASE_WORKER_ACCOUNT",
                account_id.to_string(),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn isolated operation worker");
        let stdout = child.stdout.take().expect("worker stdout");
        let mut lines = BufReader::new(stdout).lines();
        timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.expect("read worker stdout") {
                if line == READY {
                    return;
                }
            }
            panic!("operation worker exited before acquiring its lease");
        })
        .await
        .expect("operation worker did not become ready");
        child
    }

    /// A subprocess owns the lease beyond the first expiry window.  Its
    /// heartbeat must keep a second process from stealing it, then a crashed
    /// worker must recover after the durable timeout.
    #[tokio::test]
    async fn active_cross_process_lease_renews_and_crash_recovers() {
        let directory = TempDir::new().expect("temporary profile");
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.expect("open profile");
        let account = account(&store, "cross-process@example.test").await;
        let coordinator = coordinator_with_duration(&store, TEST_LEASE_DURATION).await;
        let mut worker = spawn_worker(&database, account.id).await;

        sleep(TEST_LEASE_DURATION + Duration::from_millis(250)).await;
        let outcome = coordinator
            .acquire(account.id, AccountOperationKind::ProviderMutation)
            .await
            .expect("respect renewed worker lease");
        assert!(matches!(outcome, AccountOperationAcquire::Busy));

        worker.start_kill().expect("stop worker");
        timeout(Duration::from_secs(3), worker.wait())
            .await
            .expect("reap worker before deadline")
            .expect("worker process exit");

        let outcome = coordinator
            .acquire(account.id, AccountOperationKind::ProviderMutation)
            .await
            .expect("preserve lease briefly after crash");
        assert!(matches!(outcome, AccountOperationAcquire::Busy));

        sleep(TEST_LEASE_DURATION + Duration::from_millis(250)).await;
        let recovered = acquire(
            &coordinator,
            account.id,
            AccountOperationKind::ProviderMutation,
        )
        .await;
        recovered.release().await.expect("release recovered lease");
    }

    /// Worker half of `active_cross_process_lease_renews_and_crash_recovers`.
    #[tokio::test]
    async fn cross_process_operation_lease_worker() {
        let Ok(database) = std::env::var("DAKIA_OPERATION_LEASE_WORKER_DATABASE") else {
            return;
        };
        let account_id = std::env::var("DAKIA_OPERATION_LEASE_WORKER_ACCOUNT")
            .expect("worker account configured")
            .parse::<AccountId>()
            .expect("worker account id is valid");
        let store = Store::open(database).await.expect("worker opens profile");
        let coordinator = coordinator_with_duration(&store, TEST_LEASE_DURATION).await;
        let _lease = acquire(
            &coordinator,
            account_id,
            AccountOperationKind::ProviderMutation,
        )
        .await;
        println!("{READY}");
        sleep(Duration::from_secs(10)).await;
    }
}
