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
            bail!("OAuth authentication failed ({status}): {body}");
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
