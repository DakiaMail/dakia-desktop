use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type AccountId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AccountAuth {
    Password {
        username: String,
    },
    OAuth2 {
        username: String,
        provider: String,
        access_token_expires_at: Option<DateTime<Utc>>,
    },
}

impl AccountAuth {
    pub fn username(&self) -> &str {
        match self {
            Self::Password { username } | Self::OAuth2 { username, .. } => username,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    pub id: AccountId,
    pub email: String,
    /// A device-local label used to distinguish accounts in Dakia's UI.
    #[serde(default)]
    pub account_name: String,
    pub display_name: String,
    pub provider_id: String,
    pub auth: AccountAuth,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: crate::provider::Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: crate::provider::Security,
    pub archive_mailbox: String,
    pub spam_mailbox: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl Account {
    /// Accounts stored before local account names were introduced have no
    /// value. Their stable default is the account's email address.
    pub fn ensure_account_name(&mut self) {
        if self.account_name.trim().is_empty() {
            self.account_name = self.email.clone();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountDraft {
    pub email: String,
    pub display_name: String,
    pub provider_id: Option<String>,
    pub username: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<crate::provider::Security>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<crate::provider::Security>,
    pub archive_mailbox: Option<String>,
    pub spam_mailbox: Option<String>,
}

impl AccountDraft {
    pub fn into_account(self, preset: &crate::provider::ProviderPreset) -> Account {
        let username = self.username.unwrap_or_else(|| self.email.clone());
        Account {
            id: Uuid::new_v4(),
            account_name: self.email.clone(),
            email: self.email,
            display_name: self.display_name,
            provider_id: preset.id.to_owned(),
            auth: AccountAuth::Password { username },
            imap_host: self
                .imap_host
                .unwrap_or_else(|| preset.imap_host.to_owned()),
            imap_port: self.imap_port.unwrap_or(preset.imap_port),
            imap_security: self.imap_security.unwrap_or(preset.imap_security),
            smtp_host: self
                .smtp_host
                .unwrap_or_else(|| preset.smtp_host.to_owned()),
            smtp_port: self.smtp_port.unwrap_or(preset.smtp_port),
            smtp_security: self.smtp_security.unwrap_or(preset.smtp_security),
            archive_mailbox: self
                .archive_mailbox
                .unwrap_or_else(|| preset.archive_mailbox.to_owned()),
            spam_mailbox: self
                .spam_mailbox
                .unwrap_or_else(|| preset.spam_mailbox.to_owned()),
            enabled: true,
            created_at: Utc::now(),
        }
    }
}
