use anyhow::{bail, Context, Result};
use chrono::Utc;
use dakia_core::{Account, AccountAuth, MailService, Security, Store};
use serde::Deserialize;
use std::{collections::BTreeMap, env, process::ExitCode, time::Duration};
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

const PROVIDER_SMOKE_TIMEOUT: Duration = Duration::from_secs(45);
const SMOKE_SUCCESS_MESSAGE: &str =
    "Provider smoke passed: IMAP read-neutral sync and SMTP auth/QUIT completed.";
const SMOKE_FAILURE_MESSAGE: &str =
    "Provider smoke failed; no remote mailbox mutation was requested.";

#[derive(Deserialize)]
struct ProviderSmokeConfig {
    version: u8,
    provider: String,
    #[serde(rename = "accountEmail")]
    account_email: String,
    imap: Endpoint,
    smtp: Endpoint,
    credentials: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Endpoint {
    host: String,
    port: u16,
    security: EndpointSecurity,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum EndpointSecurity {
    Tls,
    Starttls,
}

impl From<EndpointSecurity> for Security {
    fn from(value: EndpointSecurity) -> Self {
        match value {
            EndpointSecurity::Tls => Self::Tls,
            EndpointSecurity::Starttls => Self::StartTls,
        }
    }
}

impl ProviderSmokeConfig {
    fn password(&self) -> Result<&str> {
        if self.version != 1
            || self.provider.trim().is_empty()
            || self.account_email.trim().is_empty()
            || self.imap.host.trim().is_empty()
            || self.smtp.host.trim().is_empty()
        {
            bail!("invalid provider smoke configuration");
        }
        let Some((kind, secret)) = self.credentials.iter().next() else {
            bail!("invalid provider smoke credentials");
        };
        if self.credentials.len() != 1
            || !matches!(kind.as_str(), "password" | "appPassword")
            || secret.trim().is_empty()
        {
            // OAuth and other credential kinds are intentionally rejected:
            // this narrow live smoke must not invent a refresh/token flow.
            bail!("unsupported provider smoke credential kind");
        }
        Ok(secret)
    }

    fn account(&self) -> Account {
        Account {
            id: Uuid::new_v4(),
            email: self.account_email.clone(),
            account_name: self.account_email.clone(),
            display_name: "Provider smoke".into(),
            provider_id: self.provider.clone(),
            auth: AccountAuth::Password {
                username: self.account_email.clone(),
            },
            imap_host: self.imap.host.clone(),
            imap_port: self.imap.port,
            imap_security: self.imap.security.into(),
            smtp_host: self.smtp.host.clone(),
            smtp_port: self.smtp.port,
            smtp_security: self.smtp.security.into(),
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        }
    }
}

async fn run_smoke(config: ProviderSmokeConfig, temp_dir: &TempDir) -> Result<()> {
    let password = config.password()?.to_owned();
    let account = config.account();
    let store = Store::open(temp_dir.path().join("provider-smoke.db")).await?;
    store.save_account(&account).await?;
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, &password)
        .await?;

    // The public IMAP probe authenticates and performs only CAPABILITY, LIST,
    // read-only EXAMINE INBOX, and constant-size STATUS. It fetches no message
    // content and cannot expand its remote work with mailbox size.
    service.imap_auth_probe(&account).await?;
    // This authenticates over the configured TLS/STARTTLS path and issues
    // QUIT before MAIL/RCPT/DATA, so no SMTP envelope or body is submitted.
    service.smtp_auth_probe(&account).await?;
    Ok(())
}

async fn execute() -> Result<()> {
    let raw = env::var("PROVIDER_SMOKE_CONFIG")
        .map_err(|_| anyhow::anyhow!("provider smoke configuration is unavailable"))?;
    let config: ProviderSmokeConfig = serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("invalid provider smoke configuration"))?;
    let temp_dir = tempfile::Builder::new()
        .prefix("dakia-provider-smoke-")
        .tempdir()?;
    let outcome = timeout(PROVIDER_SMOKE_TIMEOUT, run_smoke(config, &temp_dir)).await;
    // The temporary SQLite database also contains the encrypted credential.
    // Remove it regardless of a timeout, auth failure, or protocol error.
    temp_dir
        .close()
        .context("could not remove temporary provider smoke state")?;
    outcome.map_err(|_| anyhow::anyhow!("provider smoke timed out"))?
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => {
            println!("{SMOKE_SUCCESS_MESSAGE}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            // Never include the secret-backed configuration, endpoint, or
            // account identity in CI logs.
            eprintln!("{SMOKE_FAILURE_MESSAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "must-not-appear-in-provider-smoke-output";

    #[test]
    fn live_smoke_rejects_oauth_style_credentials_without_echoing_their_value() {
        let config: ProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "test-provider",
            "accountEmail": "smoke@example.invalid",
            "imap": { "host": "imap.example.invalid", "port": 993, "security": "tls" },
            "smtp": { "host": "smtp.example.invalid", "port": 465, "security": "tls" },
            "credentials": { "accessToken": SECRET },
        }))
        .unwrap();

        let error = config.password().unwrap_err().to_string();
        assert!(error.contains("unsupported provider smoke credential kind"));
        assert!(!error.contains(SECRET));
    }

    #[test]
    fn binary_log_messages_are_static_and_do_not_echo_config_values() {
        for message in [SMOKE_SUCCESS_MESSAGE, SMOKE_FAILURE_MESSAGE] {
            assert!(!message.contains(SECRET));
            assert!(!message.contains("example.invalid"));
            assert!(!message.contains("host"));
        }
    }
}
