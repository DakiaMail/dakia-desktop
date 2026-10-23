use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

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
        self.should_refresh_at(Utc::now())
    }

    fn should_refresh_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at
            .map(|expiry| expiry <= now + Duration::minutes(5))
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

    pub(crate) async fn refresh_with_client(&mut self, client: &reqwest::Client) -> Result<()> {
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
            // Provider responses can contain echoed credentials, request
            // details, or other private diagnostics. The explicit OAuth code
            // was already classified above, so keep the stable terminal-auth
            // signal without copying provider-controlled text into logs or UI.
            let safe_code = match provider_error.as_deref() {
                Some("invalid_grant") => "invalid_grant",
                Some("invalid_client") => "invalid_client",
                _ => unreachable!("only explicit credential errors are terminal"),
            };
            bail!("OAuth authentication failed ({status}, {safe_code})");
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
    use crate::{
        account::{AccountAuth, AccountDraft},
        mail::MailService,
        provider,
        storage::Store,
    };
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinSet,
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

    async fn read_http_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(
                read > 0,
                "OAuth client closed before sending a complete request"
            );
            request.extend_from_slice(&buffer[..read]);
            let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
            else {
                continue;
            };
            let headers_end = headers_end + 4;
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap_or(0);
            if request.len() >= headers_end + content_length {
                return request;
            }
        }
    }

    async fn write_json_response(socket: &mut TcpStream, status: &str, body: &str) {
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    fn legacy_oauth_account() -> crate::account::Account {
        let mut account = AccountDraft {
            email: "legacy@gmail.com".into(),
            display_name: "Legacy OAuth".into(),
            provider_id: Some("gmail".into()),
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
        .into_account(provider::by_id("gmail").unwrap());
        account.auth = AccountAuth::OAuth2 {
            username: account.email.clone(),
            provider: "gmail".into(),
            access_token_expires_at: Some(Utc::now() - Duration::from_secs(60)),
        };
        account
    }

    #[test]
    fn refresh_decision_has_a_deterministic_five_minute_boundary() {
        let now = Utc::now();
        let mut tokens = expired_tokens(Url::parse("https://example.test/token").unwrap());

        tokens.expires_at = None;
        assert!(!tokens.should_refresh_at(now));
        tokens.expires_at = Some(now + chrono::Duration::minutes(5));
        assert!(tokens.should_refresh_at(now));
        tokens.expires_at =
            Some(now + chrono::Duration::minutes(5) + chrono::Duration::milliseconds(1));
        assert!(!tokens.should_refresh_at(now));
        tokens.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(tokens.should_refresh_at(now));
    }

    #[tokio::test]
    async fn successful_refresh_rotates_tokens_and_preserves_rotation_when_omitted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let first_request = read_http_request(&mut first).await;
            write_json_response(
                &mut first,
                "200 OK",
                r#"{"access_token":"access-one","refresh_token":"refresh-one","expires_in":3600}"#,
            )
            .await;
            let (mut second, _) = listener.accept().await.unwrap();
            let second_request = read_http_request(&mut second).await;
            write_json_response(
                &mut second,
                "200 OK",
                r#"{"access_token":"access-two","expires_in":1800}"#,
            )
            .await;
            (first_request, second_request)
        });
        let client = oauth_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let mut tokens = expired_tokens(Url::parse(&format!("http://{address}/token")).unwrap());

        tokens.refresh_with_client(&client).await.unwrap();
        assert_eq!(tokens.access_token, "access-one");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-one"));
        tokens.refresh_with_client(&client).await.unwrap();
        assert_eq!(tokens.access_token, "access-two");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-one"));

        let (first_request, second_request) = server.await.unwrap();
        for request in [first_request, second_request] {
            let request = String::from_utf8(request).unwrap();
            assert!(request.contains("grant_type=refresh_token"));
            assert!(request.contains("client_id=client"));
        }
    }

    #[tokio::test]
    async fn failed_refresh_preserves_the_persisted_credential() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_request(&mut socket).await;
            write_json_response(
                &mut socket,
                "400 Bad Request",
                r#"{"error":"invalid_grant"}"#,
            )
            .await;
        });
        let store = Store::in_memory().await.unwrap();
        let account = legacy_oauth_account();
        let secret_name = format!("dev.dakia.mail:{}:{}", account.id, account.auth.username());
        let original = OAuthTokens {
            access_token: "expired-access".into(),
            refresh_token: Some("recoverable-refresh".into()),
            expires_at: Some(Utc::now() - chrono::Duration::minutes(1)),
            client_id: "client".into(),
            client_secret: None,
            token_url: Url::parse(&format!("http://{address}/token")).unwrap(),
        };
        let original_json = serde_json::to_string(&original).unwrap();
        store
            .save_account_with_secret(&account, &secret_name, &original_json)
            .await
            .unwrap();
        let client = oauth_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();

        let error = MailService::new(store.clone())
            .credentials()
            .secret_with_client(&account, &client)
            .await
            .unwrap_err();
        server.await.unwrap();

        assert!(error.to_string().contains("OAuth authentication failed"));
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some(original_json.as_str())
        );
    }

    #[tokio::test]
    async fn invalid_grant_from_losing_refresh_uses_the_durable_winning_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let store = Store::in_memory().await.unwrap();
        let account = legacy_oauth_account();
        let secret_name = format!("dev.dakia.mail:{}:{}", account.id, account.auth.username());
        let initial = OAuthTokens {
            access_token: "expired-access".into(),
            refresh_token: Some("original-refresh".into()),
            expires_at: Some(Utc::now() - chrono::Duration::minutes(1)),
            client_id: "client".into(),
            client_secret: None,
            token_url: Url::parse(&format!("http://{address}/token")).unwrap(),
        };
        store
            .save_account_with_secret(
                &account,
                &secret_name,
                &serde_json::to_string(&initial).unwrap(),
            )
            .await
            .unwrap();

        let accept = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let first_request = read_http_request(&mut first).await;
            let (mut second, _) = listener.accept().await.unwrap();
            let second_request = read_http_request(&mut second).await;
            (first, first_request, second, second_request)
        });
        let credentials = MailService::new(store.clone()).credentials().clone();
        let mut refreshes = JoinSet::new();
        for _ in 0..2 {
            let credentials = credentials.clone();
            let account = account.clone();
            refreshes.spawn(async move {
                let client = oauth_client(
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    Duration::from_secs(1),
                )
                .unwrap();
                credentials.secret_with_client(&account, &client).await
            });
        }
        let (mut winner_socket, winner_request, mut loser_socket, loser_request) =
            accept.await.unwrap();
        assert!(String::from_utf8(winner_request)
            .unwrap()
            .contains("refresh_token=original-refresh"));
        assert!(String::from_utf8(loser_request)
            .unwrap()
            .contains("refresh_token=original-refresh"));

        write_json_response(
            &mut winner_socket,
            "200 OK",
            r#"{"access_token":"winning-access","refresh_token":"winning-refresh","expires_in":3600}"#,
        )
        .await;
        drop(winner_socket);
        assert_eq!(
            refreshes.join_next().await.unwrap().unwrap().unwrap(),
            "winning-access"
        );
        write_json_response(
            &mut loser_socket,
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"the original refresh token was rotated"}"#,
        )
        .await;
        drop(loser_socket);
        assert_eq!(
            refreshes.join_next().await.unwrap().unwrap().unwrap(),
            "winning-access",
            "the late refresh must use the already-persisted winner"
        );

        let persisted: OAuthTokens = serde_json::from_str(
            store
                .secret(&secret_name)
                .await
                .unwrap()
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.access_token, "winning-access");
        assert_eq!(persisted.refresh_token.as_deref(), Some("winning-refresh"));
    }

    #[tokio::test]
    async fn app_password_conversion_and_account_removal_fence_late_refreshes() {
        let store = Store::in_memory().await.unwrap();
        let oauth = legacy_oauth_account();
        let secret_name = format!("dev.dakia.mail:{}:{}", oauth.id, oauth.auth.username());
        store
            .save_account_with_secret(&oauth, &secret_name, "old-oauth-json")
            .await
            .unwrap();

        let mut converted = oauth.clone();
        converted.auth = AccountAuth::Password {
            username: oauth.auth.username().into(),
        };
        store
            .save_account_with_secret(&converted, &secret_name, "new-app-password")
            .await
            .unwrap();
        assert!(store
            .compare_and_set_oauth_secret_if_current(
                oauth.id,
                &secret_name,
                "old-oauth-json",
                "late-refresh-json",
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store.secret(&secret_name).await.unwrap().as_deref(),
            Some("new-app-password")
        );

        let mut removed = legacy_oauth_account();
        removed.id = uuid::Uuid::new_v4();
        removed.email = "removed-oauth@example.test".into();
        let removed_secret_name =
            format!("dev.dakia.mail:{}:{}", removed.id, removed.auth.username());
        store
            .save_account_with_secret(&removed, &removed_secret_name, "removed-oauth-json")
            .await
            .unwrap();
        store.delete_account(removed.id).await.unwrap();
        assert!(store
            .compare_and_set_oauth_secret_if_current(
                removed.id,
                &removed_secret_name,
                "removed-oauth-json",
                "late-removed-refresh-json",
            )
            .await
            .unwrap()
            .is_none());
        assert!(
            store.secret(&removed_secret_name).await.unwrap().is_none(),
            "account removal atomically deletes its credential, so a stale refresh has no secret to mutate"
        );
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
            read_http_request(&mut socket).await;
            write_json_response(
                &mut socket,
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_description":"access-token-canary refresh-token-canary provider-debug-canary"}"#,
            )
            .await;
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
        for secret in [
            "access-token-canary",
            "refresh-token-canary",
            "provider-debug-canary",
        ] {
            assert!(
                !error.to_string().contains(secret),
                "provider-controlled OAuth error text leaked {secret}"
            );
        }
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
