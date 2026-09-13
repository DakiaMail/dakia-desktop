use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use url::Url;

use crate::{account::Account, storage::Store};

type RefreshLock = tokio::sync::Mutex<Option<String>>;

fn refresh_lock(secret_name: &str) -> Arc<RefreshLock> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<RefreshLock>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .expect("OAuth refresh locks poisoned");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(secret_name).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(None));
    locks.insert(secret_name.to_owned(), Arc::downgrade(&lock));
    lock
}

/// Independent IMAP and SMTP services share refresh work. Read the durable
/// token *after* obtaining the lock so a waiter uses the winner's rotated token.
/// A failed refresh is also shared by its current waiters, avoiding a burst of
/// identical refresh requests during a provider outage.
pub(crate) async fn current_access_token(
    store: &Store,
    account: &Account,
    secret_name: &str,
) -> Result<String> {
    let lock = refresh_lock(secret_name);
    let mut failure = lock.lock().await;
    if let Some(error) = failure.as_ref() {
        bail!("{error}");
    }
    let mut stored = store
        .secret_for_account(account, secret_name)
        .await?
        .context("credentials are not stored for this account")
        .context("OAuth authentication failed")?;
    let mut tokens: OAuthTokens = serde_json::from_str(&stored)
        .context("stored OAuth credentials are invalid")
        .context("OAuth authentication failed")?;
    if tokens.should_refresh() {
        let waiting_since = std::time::Instant::now();
        let lease = loop {
            if let Some(lease) = store
                .acquire_oauth_refresh_lease(account, secret_name)
                .await?
            {
                break lease;
            }
            store
                .secret_for_account(account, secret_name)
                .await?
                .context("OAuth credentials are no longer available")?;
            if waiting_since.elapsed() > std::time::Duration::from_secs(125) {
                bail!("OAuth refresh is still in progress; try again");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        // Another process may have completed rotation while we waited.
        stored = store
            .secret_for_account(account, secret_name)
            .await?
            .context("OAuth credentials are no longer available")?;
        tokens = serde_json::from_str(&stored).context("stored OAuth credentials are invalid")?;
        if !tokens.should_refresh() {
            return Ok(tokens.access_token);
        }
        if let Err(error) = tokens.refresh().await {
            *failure = Some(error.to_string());
            return Err(error);
        }
        if !store
            .replace_oauth_secret_for_account(
                account,
                secret_name,
                &stored,
                &serde_json::to_string(&tokens)?,
                &lease,
            )
            .await?
        {
            bail!("Account credentials changed during refresh; try again");
        }
    }
    Ok(tokens.access_token)
}

const OAUTH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const OAUTH_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Credentials retained solely to keep accounts connected before OAuth sign-in
/// was removed working. New OAuth grants are intentionally unsupported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OAuthTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) client_id: String,
    #[serde(default)]
    pub(crate) client_secret: Option<String>,
    pub(crate) token_url: Url,
}

impl OAuthTokens {
    pub(crate) fn should_refresh(&self) -> bool {
        self.expires_at
            .map(|expiry| expiry <= Utc::now() + Duration::minutes(5))
            .unwrap_or(false)
    }

    pub(crate) async fn refresh(&mut self) -> Result<()> {
        let client = oauth_client(
            OAUTH_CONNECT_TIMEOUT,
            OAUTH_REQUEST_TIMEOUT,
            OAUTH_READ_TIMEOUT,
        )?;
        self.refresh_with_client(&client).await
    }

    async fn refresh_with_client(&mut self, client: &reqwest::Client) -> Result<()> {
        let refresh_token = self.refresh_token.as_deref().ok_or_else(|| {
            anyhow!("OAuth authentication failed: access token expired and no refresh token is available")
        })?;
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if let Some(client_secret) = self.client_secret.as_deref() {
            form.push(("client_secret", client_secret));
        }
        let response = client
            .post(self.token_url.clone())
            .form(&form)
            .send()
            .await?;
        let response = oauth_response(response).await?;
        self.access_token = response.access_token;
        if response.refresh_token.is_some() {
            self.refresh_token = response.refresh_token;
        }
        self.expires_at = response
            .expires_in
            .map(|seconds| Utc::now() + Duration::seconds(seconds));
        Ok(())
    }
}

fn oauth_client(
    connect_timeout: std::time::Duration,
    request_timeout: std::time::Duration,
    read_timeout: std::time::Duration,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .read_timeout(read_timeout)
        .build()
}

async fn oauth_response(response: reqwest::Response) -> Result<TokenResponse> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        let provider_error = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_owned));
        if refresh_failure_kind(status, provider_error.as_deref())
            == RefreshFailureKind::Authentication
        {
            bail!("OAuth authentication failed ({status})");
        }
        // Do not include provider-controlled text in the retryable error.
        // Realtime sync still recognizes legacy authentication rejections by
        // their stable error prefix, so an error description containing the
        // word "authentication" must not turn a temporary outage into a
        // permanent account pause.
        bail!("OAuth token refresh failed ({status})");
    }
    serde_json::from_str(&body).context("OAuth token response was invalid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshFailureKind {
    Authentication,
    Connection,
}

fn refresh_failure_kind(
    _status: reqwest::StatusCode,
    provider_error: Option<&str>,
) -> RefreshFailureKind {
    if matches!(provider_error, Some("invalid_grant" | "invalid_client")) {
        RefreshFailureKind::Authentication
    } else {
        // OAuth providers legitimately use client status codes for temporary
        // throttling and availability errors. Only the explicit credential
        // errors above are terminal authentication failures.
        RefreshFailureKind::Connection
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn expired_tokens(token_url: Url) -> OAuthTokens {
        OAuthTokens {
            access_token: "expired".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expires_at: None,
            client_id: "client".to_owned(),
            client_secret: None,
            token_url,
        }
    }

    #[tokio::test]
    async fn durable_refresh_lease_waits_for_another_store_and_reuses_its_rotation() {
        use crate::{
            account::{AccountAuth, AccountDraft},
            provider,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth.db");
        let store = Store::open(&path).await.unwrap();
        let mut account = serde_json::from_value::<AccountDraft>(serde_json::json!({
            "email": "lease@example.test", "display_name": "Lease", "provider_id": "fastmail"
        }))
        .unwrap()
        .into_account(provider::by_id("fastmail").unwrap());
        account.auth = AccountAuth::OAuth2 {
            username: account.email.clone(),
            provider: "gmail".into(),
            access_token_expires_at: None,
        };
        store.save_account(&account).await.unwrap();
        let name = format!("dev.dakia.mail:{}:{}", account.id, account.email);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut tokens = expired_tokens(
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap(),
        );
        tokens.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        let original = serde_json::to_string(&tokens).unwrap();
        store.set_secret(&name, &original).await.unwrap();
        let other_process_store = Store::open(&path).await.unwrap();
        let lease = other_process_store
            .acquire_oauth_refresh_lease(&account, &name)
            .await
            .unwrap()
            .unwrap();
        let waiting = tokio::spawn({
            let (store, account, name) = (store.clone(), account.clone(), name.clone());
            async move { current_access_token(&store, &account, &name).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "another store owns token rotation");
        tokens.access_token = "other-process-access".into();
        tokens.refresh_token = Some("other-process-rotated-refresh".into());
        tokens.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        assert!(other_process_store
            .replace_oauth_secret_for_account(
                &account,
                &name,
                &original,
                &serde_json::to_string(&tokens).unwrap(),
                &lease
            )
            .await
            .unwrap());
        drop(lease);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "other-process-access"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "the waiting process must not rotate the old token again"
        );
    }

    #[tokio::test]
    async fn concurrent_services_share_one_refresh_and_persist_rotated_tokens() {
        use crate::{
            account::{AccountAuth, AccountDraft},
            provider,
        };
        let store = Store::in_memory().await.unwrap();
        let mut account = AccountDraft {
            email: "reader@example.test".into(),
            display_name: "Reader".into(),
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
        .into_account(provider::by_id("fastmail").unwrap());
        account.auth = AccountAuth::OAuth2 {
            username: account.email.clone(),
            provider: "gmail".into(),
            access_token_expires_at: None,
        };
        store.save_account(&account).await.unwrap();
        let name = format!("dev.dakia.mail:{}:{}", account.id, account.email);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut tokens = expired_tokens(
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap(),
        );
        tokens.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        store
            .set_secret(&name, &serde_json::to_string(&tokens).unwrap())
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            // Hold the first response while all eight independent clients queue.
            tokio::time::sleep(Duration::from_millis(30)).await;
            let body = r#"{"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "only one refresh request is allowed"
            );
        });
        let mut clients = Vec::new();
        for _ in 0..8 {
            let (store, account, name) = (store.clone(), account.clone(), name.clone());
            clients.push(tokio::spawn(async move {
                current_access_token(&store, &account, &name).await.unwrap()
            }));
        }
        for client in clients {
            assert_eq!(client.await.unwrap(), "new-access");
        }
        server.await.unwrap();
        let saved: OAuthTokens =
            serde_json::from_str(&store.secret(&name).await.unwrap().unwrap()).unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("rotated-refresh"));
    }

    #[tokio::test]
    async fn refresh_cannot_restore_credentials_after_account_removal() {
        use crate::{
            account::{AccountAuth, AccountDraft},
            provider,
        };
        let store = Store::in_memory().await.unwrap();
        let mut account = AccountDraft {
            email: "removed@example.test".into(),
            display_name: "Removed".into(),
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
        .into_account(provider::by_id("fastmail").unwrap());
        account.auth = AccountAuth::OAuth2 {
            username: account.email.clone(),
            provider: "gmail".into(),
            access_token_expires_at: None,
        };
        store.save_account(&account).await.unwrap();
        let name = format!("dev.dakia.mail:{}:{}", account.id, account.email);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut tokens = expired_tokens(
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap(),
        );
        tokens.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        store
            .set_secret(&name, &serde_json::to_string(&tokens).unwrap())
            .await
            .unwrap();
        let (started, requested) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            started.send(()).unwrap();
            released.await.unwrap();
            let body = r#"{"access_token":"late-access","expires_in":3600}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = tokio::spawn({
            let (store, account, name) = (store.clone(), account.clone(), name.clone());
            async move { current_access_token(&store, &account, &name).await }
        });
        requested.await.unwrap();
        store.delete_account(account.id).await.unwrap();
        store.delete_secret(&name).await.unwrap();
        release.send(()).unwrap();
        assert!(client.await.unwrap().is_err());
        server.await.unwrap();
        assert!(store.secret(&name).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_refresh_token_is_an_authentication_failure() {
        let mut tokens = expired_tokens(Url::parse("https://example.test/token").unwrap());
        tokens.refresh_token = None;

        let error = tokens.refresh().await.unwrap_err();

        assert!(error.to_string().contains("OAuth authentication failed"));
    }

    #[tokio::test]
    async fn rejected_refresh_token_is_an_authentication_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 25\r\n\r\n{\"error\":\"invalid_grant\"}")
                .await
                .unwrap();
        });
        let client = oauth_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let mut tokens = expired_tokens(Url::parse(&format!("http://{address}/token")).unwrap());

        let error = tokens.refresh_with_client(&client).await.unwrap_err();
        server.await.unwrap();

        assert!(error.to_string().contains("OAuth authentication failed"));
        assert_eq!(tokens.access_token, "expired");
    }

    #[tokio::test]
    async fn unavailable_token_service_description_cannot_spoof_an_authentication_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = r#"{"error":"temporarily_unavailable","error_description":"Authentication service temporarily unavailable"}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = oauth_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let mut tokens = expired_tokens(Url::parse(&format!("http://{address}/token")).unwrap());

        let error = tokens.refresh_with_client(&client).await.unwrap_err();
        server.await.unwrap();

        assert!(!error
            .to_string()
            .to_ascii_lowercase()
            .contains("authentication"));
        assert!(error.to_string().contains("OAuth token refresh failed"));
    }

    #[tokio::test]
    async fn unreachable_token_service_remains_a_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = oauth_client(
            Duration::from_millis(100),
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap();
        let mut tokens = expired_tokens(Url::parse(&format!("http://{address}/token")).unwrap());

        let error = tokens.refresh_with_client(&client).await.unwrap_err();

        assert!(!error.to_string().contains("OAuth authentication failed"));
        assert_eq!(tokens.access_token, "expired");
    }

    #[test]
    fn temporary_refresh_failures_are_not_misclassified_as_bad_credentials() {
        for (status, provider_error) in [
            (reqwest::StatusCode::REQUEST_TIMEOUT, None),
            (reqwest::StatusCode::TOO_MANY_REQUESTS, None),
            (
                reqwest::StatusCode::BAD_REQUEST,
                Some("temporarily_unavailable"),
            ),
            (
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                Some("server_error"),
            ),
        ] {
            assert!(status.is_client_error() || status.is_server_error());
            assert!(
                refresh_failure_kind(status, provider_error) == RefreshFailureKind::Connection,
                "{status} / {provider_error:?} must remain retryable"
            );
        }
        assert_eq!(
            refresh_failure_kind(reqwest::StatusCode::BAD_REQUEST, Some("invalid_grant")),
            RefreshFailureKind::Authentication
        );
        assert_eq!(
            refresh_failure_kind(reqwest::StatusCode::UNAUTHORIZED, Some("invalid_client")),
            RefreshFailureKind::Authentication
        );
    }
}
