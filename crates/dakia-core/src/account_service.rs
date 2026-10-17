//! Shared account lifecycle operations for the desktop app and command line.
//!
//! This module deliberately owns the credential and reset-index ordering.  A
//! caller should not need to know which account changes invalidate the local
//! IMAP UID namespace, or risk saving account metadata without its password.

use crate::{
    operations::AccountOperationKind, provider, Account, AccountAuth, AccountDraft, AccountId,
    MailRebuildJob, MailService, Store,
};
use anyhow::{bail, Context, Result};

#[derive(Clone)]
pub struct AccountService {
    store: Store,
}

impl AccountService {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Creates a password-authenticated account and stores its credential in
    /// the profile vault.  Provider presets fill unspecified connection
    /// details, while custom accounts must provide both hosts explicitly.
    pub async fn add(&self, draft: AccountDraft, password: &str) -> Result<Account> {
        self.add_with_account_name(draft, None, password).await
    }

    /// Same as [`Self::add`], with an optional human-facing account label.
    pub async fn add_with_account_name(
        &self,
        draft: AccountDraft,
        account_name: Option<String>,
        password: &str,
    ) -> Result<Account> {
        if password.trim().is_empty() {
            bail!("a password or app password is required");
        }
        let preset = preset_for_draft(&draft)?;
        let mut account = draft.into_account(preset);
        if let Some(account_name) = account_name {
            account.account_name = account_name.trim().to_owned();
        }
        validate_account(&account)?;
        ensure_unique_email(&self.store.accounts().await?, &account, None)?;

        let mail = MailService::new(self.store.clone());
        mail.credentials().set_password(&account, password).await?;
        let job = reset_job(account.id);
        if let Err(error) = self
            .store
            .save_account_with_reset_mail_rebuild_job(&account, &job)
            .await
        {
            let _ = mail.credentials().delete(&account).await;
            return Err(error).context("could not save account");
        }
        Ok(account)
    }

    /// Replaces the persisted account.  Supplying a password replaces the
    /// encrypted credential; omitting it preserves the current one.
    pub async fn update(&self, account: Account, password: Option<&str>) -> Result<Account> {
        validate_account(&account)?;
        let existing = self
            .store
            .account(account.id)
            .await?
            .context("account not found")?;
        let _lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(existing.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            let existing = self
                .store
                .account(account.id)
                .await?
                .context("account not found")?;
            ensure_unique_email(&self.store.accounts().await?, &account, Some(account.id))?;

            let namespace_changed = !same_mail_namespace(&existing, &account);
            let mail = MailService::new(self.store.clone());
            let password = password.filter(|value| !value.trim().is_empty());
            if password.is_some() && !matches!(account.auth, AccountAuth::Password { .. }) {
                bail!("password credentials cannot be used with OAuth accounts");
            }

            let new_secret_name = credential_secret_name(&account);
            let old_secret_name = credential_secret_name(&existing);
            if password.is_none() && old_secret_name != new_secret_name {
                bail!(
                    "changing the account username requires a replacement password or app password"
                );
            }
            // Read the old values before replacing a credential.  `set_password`
            // uses the same deterministic name when the username is unchanged,
            // so reading it afterwards would only capture the new secret.
            let previous_old_secret = self.store.secret(&old_secret_name).await?;
            let previous_new_secret = if old_secret_name != new_secret_name {
                self.store.secret(&new_secret_name).await?
            } else {
                None
            };
            if let Some(password) = password {
                mail.credentials().set_password(&account, password).await?;
            }

            let persist = if namespace_changed {
                self.store
                    .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                        &account,
                        &reset_job(account.id),
                        (old_secret_name != new_secret_name).then_some(old_secret_name.as_str()),
                        &new_secret_name,
                    )
                    .await
            } else {
                self.store.save_account(&account).await
            };
            if let Err(error) = persist {
                // The old secret stays authoritative unless it has already been
                // atomically removed with a successful metadata commit.
                if password.is_some() {
                    restore_secret(
                        &self.store,
                        &old_secret_name,
                        previous_old_secret.as_deref(),
                    )
                    .await?;
                    if old_secret_name != new_secret_name {
                        restore_secret(
                            &self.store,
                            &new_secret_name,
                            previous_new_secret.as_deref(),
                        )
                        .await?;
                    }
                }
                return Err(error).context("could not update account");
            }
            Ok(account)
        }
        .await;
        let release = _lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(account), Ok(())) => Ok(account),
        }
    }

    /// Enables or disables an account without changing its provider identity.
    pub async fn set_enabled(&self, account_id: AccountId, enabled: bool) -> Result<Account> {
        let account = self
            .store
            .account(account_id)
            .await?
            .context("account not found")?;
        let _lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            let mut account = self
                .store
                .account(account_id)
                .await?
                .context("account not found")?;
            account.enabled = enabled;
            self.store.save_account(&account).await?;
            Ok(account)
        }
        .await;
        let release = _lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(account), Ok(())) => Ok(account),
        }
    }

    /// Checks both IMAP and SMTP authentication without reading or changing
    /// remote message state.
    pub async fn probe(&self, account_id: AccountId) -> Result<()> {
        let account = self
            .store
            .account(account_id)
            .await?
            .context("account not found")?;
        let _lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::ProviderMutation)
            .await?;
        let result = async {
            let mail = MailService::new(self.store.clone());
            mail.imap_auth_probe(&account).await?;
            mail.smtp_auth_probe(&account).await
        }
        .await;
        let release = _lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Removes the encrypted credential and all local account-owned data.
    pub async fn remove(&self, account_id: AccountId) -> Result<()> {
        let account = self
            .store
            .account(account_id)
            .await?
            .context("account not found")?;
        let _lease = self
            .store
            .operation_coordinator()
            .await?
            .acquire_required(account.id, AccountOperationKind::AccountDeletion)
            .await?;
        let result = async {
            MailService::new(self.store.clone())
                .credentials()
                .delete(&account)
                .await?;
            self.store.delete_account(account_id).await
        }
        .await;
        let release = _lease.release().await;
        match (result, release) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

async fn restore_secret(store: &Store, name: &str, previous: Option<&str>) -> Result<()> {
    match previous {
        Some(secret) => store.set_secret(name, secret).await,
        None => store.delete_secret(name).await,
    }
}

pub fn validate_account(account: &Account) -> Result<()> {
    if account.email.trim().is_empty() || !account.email.contains('@') {
        bail!("a valid account email is required");
    }
    if account.account_name.trim().is_empty() {
        bail!("account name is required");
    }
    if account.display_name.trim().is_empty() {
        bail!("display name is required");
    }
    if account.imap_host.trim().is_empty() || account.smtp_host.trim().is_empty() {
        bail!("IMAP and SMTP hosts are required");
    }
    if account.imap_port == 0 || account.smtp_port == 0 {
        bail!("IMAP and SMTP ports must be between 1 and 65535");
    }
    if account.archive_mailbox.trim().is_empty() || account.spam_mailbox.trim().is_empty() {
        bail!("archive and spam mailboxes are required");
    }
    Ok(())
}

pub fn credential_secret_name(account: &Account) -> String {
    format!("dev.dakia.mail:{}:{}", account.id, account.auth.username())
}

fn preset_for_draft(draft: &AccountDraft) -> Result<&'static crate::ProviderPreset> {
    let preset = match draft.provider_id.as_deref() {
        Some(provider_id) => provider::by_id(provider_id)
            .with_context(|| format!("unknown provider preset: {provider_id}"))?,
        None => provider::detect(&draft.email),
    };
    if preset.id == "custom"
        && (draft
            .imap_host
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
            || draft
                .smtp_host
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty())
    {
        bail!("custom accounts require IMAP and SMTP hosts");
    }
    Ok(preset)
}

fn ensure_unique_email(
    accounts: &[Account],
    candidate: &Account,
    excluding: Option<AccountId>,
) -> Result<()> {
    if accounts.iter().any(|existing| {
        Some(existing.id) != excluding
            && existing
                .email
                .trim()
                .eq_ignore_ascii_case(candidate.email.trim())
    }) {
        bail!("this account is already connected");
    }
    Ok(())
}

fn reset_job(account_id: AccountId) -> MailRebuildJob {
    MailRebuildJob {
        account_id,
        phase: "connecting".into(),
        completed: 0,
        total: None,
        reset_before_sync: true,
    }
}

fn same_mail_namespace(existing: &Account, candidate: &Account) -> bool {
    existing.provider_id == candidate.provider_id
        && match (&existing.auth, &candidate.auth) {
            (
                AccountAuth::Password { username: left },
                AccountAuth::Password { username: right },
            ) => left == right,
            (
                AccountAuth::OAuth2 {
                    username: left,
                    provider: left_provider,
                    ..
                },
                AccountAuth::OAuth2 {
                    username: right,
                    provider: right_provider,
                    ..
                },
            ) => left == right && left_provider == right_provider,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn draft(email: &str) -> AccountDraft {
        AccountDraft {
            email: email.into(),
            display_name: "Test User".into(),
            provider_id: Some("custom".into()),
            username: None,
            imap_host: Some("imap.example.test".into()),
            imap_port: Some(993),
            imap_security: None,
            smtp_host: Some("smtp.example.test".into()),
            smtp_port: Some(465),
            smtp_security: None,
            archive_mailbox: None,
            spam_mailbox: None,
        }
    }

    #[tokio::test]
    async fn add_stores_encrypted_password_and_reset_intent() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("dakia.db"))
            .await
            .unwrap();
        let service = AccountService::new(store.clone());
        let account = service
            .add(draft("user@example.test"), "app password")
            .await
            .unwrap();

        let stored = store.accounts().await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, account.id);
        assert_eq!(store.mail_rebuild_jobs().await.unwrap().len(), 1);
        assert_eq!(
            store
                .secret(&credential_secret_name(&account))
                .await
                .unwrap()
                .as_deref(),
            Some("app password")
        );
    }

    #[tokio::test]
    async fn duplicate_email_is_rejected_without_overwriting_credential() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("dakia.db"))
            .await
            .unwrap();
        let service = AccountService::new(store.clone());
        let first = service
            .add(draft("user@example.test"), "first")
            .await
            .unwrap();
        let error = service
            .add(draft("USER@example.test"), "second")
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "this account is already connected");
        assert_eq!(store.accounts().await.unwrap().len(), 1);
        assert_eq!(
            store
                .secret(&credential_secret_name(&first))
                .await
                .unwrap()
                .as_deref(),
            Some("first")
        );
    }

    #[tokio::test]
    async fn enable_toggle_preserves_account_identity_and_secret() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("dakia.db"))
            .await
            .unwrap();
        let service = AccountService::new(store.clone());
        let account = service
            .add(draft("user@example.test"), "secret")
            .await
            .unwrap();
        let disabled = service.set_enabled(account.id, false).await.unwrap();

        assert_eq!(disabled.id, account.id);
        assert!(!disabled.enabled);
        assert_eq!(
            store
                .secret(&credential_secret_name(&account))
                .await
                .unwrap()
                .as_deref(),
            Some("secret")
        );
    }
}
