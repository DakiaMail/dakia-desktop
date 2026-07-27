use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use url::Url;
use uuid::Uuid;

const OAUTH_CALLBACK_SUCCESS_HTML: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Authorization received</title></head><body><p>Authorization received. Return to Dakia while account setup completes.</p></body></html>";
const OAUTH_CALLBACK_DENIED_HTML: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Sign-in not completed</title></head><body><p>Sign-in was not completed. You can close this window.</p></body></html>";
const OAUTH_CALLBACK_MAX_HEADER_BYTES: usize = 8192;
const OAUTH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const OAUTH_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[derive(Debug, Clone)]
pub struct OAuthProviderConfig {
    pub provider_id: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub authorization_url: Url,
    pub token_url: Url,
    pub scopes: Vec<String>,
}

impl OAuthProviderConfig {
    pub fn for_provider(provider_id: &str, client_id: String) -> Result<Self> {
        let (authorization_url, token_url, scopes) = match provider_id {
            "gmail" => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                vec!["https://mail.google.com/"],
            ),
            "outlook" => (
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                vec![
                    "offline_access",
                    "https://outlook.office.com/IMAP.AccessAsUser.All",
                    "https://outlook.office.com/SMTP.Send",
                ],
            ),
            "yahoo" => (
                "https://api.login.yahoo.com/oauth2/request_auth",
                "https://api.login.yahoo.com/oauth2/get_token",
                vec!["mail-r", "mail-w"],
            ),
            _ => bail!("OAuth is not configured for provider {provider_id}"),
        };
        Ok(Self {
            provider_id: provider_id.into(),
            client_id,
            client_secret: None,
            authorization_url: Url::parse(authorization_url)?,
            token_url: Url::parse(token_url)?,
            scopes: scopes.into_iter().map(str::to_owned).collect(),
        })
    }

    pub fn with_client_secret(mut self, client_secret: Option<String>) -> Self {
        self.client_secret = client_secret;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub token_url: Url,
}

impl OAuthTokens {
    pub fn should_refresh(&self) -> bool {
        self.expires_at
            .map(|expiry| expiry <= Utc::now() + Duration::minutes(5))
            .unwrap_or(false)
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let client = oauth_client(
            OAUTH_CONNECT_TIMEOUT,
            OAUTH_REQUEST_TIMEOUT,
            OAUTH_READ_TIMEOUT,
        )?;
        self.refresh_with_client(&client).await
    }

    async fn refresh_with_client(&mut self, client: &reqwest::Client) -> Result<()> {
        let refresh_token = self
            .refresh_token
            .as_deref()
            .context("OAuth access token expired and no refresh token is available")?;
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

pub struct OAuthFlow {
    config: OAuthProviderConfig,
    redirect_uri: String,
    state: String,
    verifier: String,
    listener: TcpListener,
}

impl OAuthFlow {
    pub async fn start(
        config: OAuthProviderConfig,
        login_hint: Option<&str>,
    ) -> Result<(Self, Url)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        // Google Desktop OAuth clients use a loopback redirect URI consisting
        // of the IP address and the dynamically allocated port. A path such as
        // `/callback` is not part of the documented loopback redirect format
        // and can be rejected before Google shows the consent screen.
        let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let state = Uuid::new_v4().to_string();
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let authorization_url =
            build_authorization_url(&config, &redirect_uri, &state, &verifier, login_hint);
        Ok((
            Self {
                config,
                redirect_uri,
                state,
                verifier,
                listener,
            },
            authorization_url,
        ))
    }

    pub async fn finish(self) -> Result<OAuthTokens> {
        let (mut socket, _) = timeout(std::time::Duration::from_secs(300), self.listener.accept())
            .await
            .context("OAuth sign-in timed out")??;
        let request = timeout(
            std::time::Duration::from_secs(10),
            read_oauth_callback_request(&mut socket),
        )
        .await??;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .context("invalid OAuth callback")?;
        let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
        let params = callback
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        if params.get("state").map(|value| value.as_ref()) != Some(self.state.as_str()) {
            bail!("OAuth state validation failed");
        }
        if let Some(message) = params
            .get("error_description")
            .or_else(|| params.get("error"))
        {
            socket.write_all(&oauth_callback_denied_response()).await?;
            socket.shutdown().await?;
            drop(socket);
            bail!("OAuth provider denied access: {message}");
        }
        let code = params
            .get("code")
            .context("OAuth callback did not include an authorization code")?;
        socket.write_all(&oauth_callback_success_response()).await?;
        socket.shutdown().await?;
        drop(socket);
        let mut form = vec![
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_ref()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        if let Some(client_secret) = self.config.client_secret.as_deref() {
            form.push(("client_secret", client_secret));
        }
        let response = oauth_client(
            OAUTH_CONNECT_TIMEOUT,
            OAUTH_REQUEST_TIMEOUT,
            OAUTH_READ_TIMEOUT,
        )?
        .post(self.config.token_url.clone())
        .form(&form)
        .send()
        .await?;
        let response = oauth_response(response).await?;
        Ok(OAuthTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: response
                .expires_in
                .map(|seconds| Utc::now() + Duration::seconds(seconds)),
            client_id: self.config.client_id,
            client_secret: self.config.client_secret,
            token_url: self.config.token_url,
        })
    }
}

async fn read_oauth_callback_request(socket: &mut tokio::net::TcpStream) -> Result<String> {
    let mut bytes = [0u8; OAUTH_CALLBACK_MAX_HEADER_BYTES];
    let mut length = 0;
    loop {
        if length == bytes.len() {
            bail!("OAuth callback headers exceeded {OAUTH_CALLBACK_MAX_HEADER_BYTES} bytes");
        }
        let count = socket.read(&mut bytes[length..]).await?;
        if count == 0 {
            bail!("OAuth callback ended before its headers were complete");
        }
        length += count;
        if bytes[..length].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return std::str::from_utf8(&bytes[..length])
                .context("OAuth callback headers were not valid UTF-8")
                .map(str::to_owned);
        }
    }
}

fn oauth_callback_success_response() -> Vec<u8> {
    oauth_callback_html_response(OAUTH_CALLBACK_SUCCESS_HTML)
}

fn oauth_callback_denied_response() -> Vec<u8> {
    oauth_callback_html_response(OAUTH_CALLBACK_DENIED_HTML)
}

fn oauth_callback_html_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    )
    .into_bytes()
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
        bail!("OAuth token exchange failed ({status}): {body}");
    }
    serde_json::from_str(&body).context("OAuth token response was invalid")
}

fn build_authorization_url(
    config: &OAuthProviderConfig,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
    login_hint: Option<&str>,
) -> Url {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut authorization_url = config.authorization_url.clone();
    let mut query = authorization_url.query_pairs_mut();
    query
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &config.scopes.join(" "))
        .append_pair("state", state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline");
    if let Some(hint) = login_hint {
        query.append_pair("login_hint", hint);
    }
    drop(query);
    authorization_url
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
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::Notify,
    };
    #[test]
    fn creates_pkce_authorization_url() {
        let config = OAuthProviderConfig::for_provider("gmail", "client-id".into()).unwrap();
        let url = build_authorization_url(
            &config,
            "http://127.0.0.1:49152",
            "state",
            "verifier",
            Some("me@example.com"),
        );
        assert!(url.as_str().contains("code_challenge="));
        assert!(url.as_str().contains("login_hint=me%40example.com"));
        assert!(!url.as_str().contains("prompt="));
        assert!(url
            .as_str()
            .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49152"));
    }
    #[test]
    fn callback_success_response_is_complete_and_non_cacheable() {
        let response = String::from_utf8(oauth_callback_success_response()).unwrap();
        assert_complete_html_callback_response(&response, OAUTH_CALLBACK_SUCCESS_HTML);
    }

    #[tokio::test]
    async fn fragmented_callback_response_closes_before_token_exchange_completes() {
        let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_address = token_listener.local_addr().unwrap();
        let release_token_response = Arc::new(Notify::new());
        let token_server_release = Arc::clone(&release_token_response);
        let token_server = tokio::spawn(async move {
            let (mut socket, _) = token_listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            token_server_release.notified().await;
            let response_body =
                "{\"access_token\":\"access\",\"refresh_token\":\"refresh\",\"expires_in\":3600}";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
            request
        });

        let config = OAuthProviderConfig::for_provider("gmail", "client-id".into()).unwrap();
        let config = OAuthProviderConfig {
            token_url: Url::parse(&format!("http://{token_address}/token")).unwrap(),
            ..config
        };
        let (flow, _) = OAuthFlow::start(config, None).await.unwrap();
        let callback_address = flow.listener.local_addr().unwrap();
        let callback_state = flow.state.clone();
        let finish = tokio::spawn(flow.finish());

        let mut browser = TcpStream::connect(callback_address).await.unwrap();
        let callback_request = format!(
            "GET /?code=authorization-code&state={callback_state} HTTP/1.1\r\nHost: {callback_address}\r\n\r\n"
        );
        let split_at = callback_request.find("state=").unwrap() + 3;
        browser
            .write_all(&callback_request.as_bytes()[..split_at])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        browser
            .write_all(&callback_request.as_bytes()[split_at..])
            .await
            .unwrap();
        browser.shutdown().await.unwrap();

        let mut callback_response = Vec::new();
        timeout(
            Duration::from_secs(1),
            browser.read_to_end(&mut callback_response),
        )
        .await
        .expect("browser should receive EOF before the token endpoint responds")
        .unwrap();
        let callback_response = String::from_utf8(callback_response).unwrap();
        assert_complete_html_callback_response(&callback_response, OAUTH_CALLBACK_SUCCESS_HTML);

        release_token_response.notify_one();
        let request = timeout(Duration::from_secs(1), token_server)
            .await
            .expect("token endpoint should receive the exchange request")
            .unwrap();
        assert!(request.starts_with("POST /token HTTP/1.1\r\n"));
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("client_id=client-id"));
        assert!(request.contains("code=authorization-code"));
        assert!(!request.contains("client_secret="));

        let tokens = timeout(Duration::from_secs(1), finish)
            .await
            .expect("token exchange should finish after the token endpoint is released")
            .unwrap()
            .unwrap();
        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
    }

    #[tokio::test]
    async fn rejected_callbacks_do_not_exchange_tokens() {
        for (callback_query, expected_error, expected_response) in [
            (
                "error=access_denied&state=wrong-state",
                "state validation failed",
                None,
            ),
            (
                "error=access_denied&state={state}",
                "provider denied access: access_denied",
                Some(OAUTH_CALLBACK_DENIED_HTML),
            ),
        ] {
            let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let token_address = token_listener.local_addr().unwrap();
            let config = OAuthProviderConfig {
                token_url: Url::parse(&format!("http://{token_address}/token")).unwrap(),
                ..OAuthProviderConfig::for_provider("gmail", "client-id".into()).unwrap()
            };
            let (flow, _) = OAuthFlow::start(config, None).await.unwrap();
            let callback_address = flow.listener.local_addr().unwrap();
            let expected_state = flow.state.clone();
            let callback_query = callback_query.replace("{state}", &expected_state);
            let finish = tokio::spawn(flow.finish());

            let mut browser = TcpStream::connect(callback_address).await.unwrap();
            browser
                .write_all(
                    format!("GET /?{callback_query} HTTP/1.1\r\nHost: {callback_address}\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            browser.shutdown().await.unwrap();
            let mut callback_response = Vec::new();
            timeout(
                Duration::from_secs(1),
                browser.read_to_end(&mut callback_response),
            )
            .await
            .expect("rejected callback should close the browser connection")
            .unwrap();
            let callback_response = String::from_utf8(callback_response).unwrap();
            if let Some(expected_response) = expected_response {
                assert_complete_html_callback_response(&callback_response, expected_response);
            } else {
                assert!(callback_response.is_empty());
            }

            let error = timeout(Duration::from_secs(1), finish)
                .await
                .expect("rejected callback should fail promptly")
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error:#}");
            assert!(
                timeout(Duration::from_millis(50), token_listener.accept())
                    .await
                    .is_err(),
                "rejected callback unexpectedly exchanged a token"
            );
        }
    }

    fn assert_complete_html_callback_response(response: &str, expected_body: &str) {
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(headers.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(headers.contains("Cache-Control: no-store\r\n"));
        assert!(headers.contains("Connection: close"));
        assert_eq!(body, expected_body);
    }

    async fn read_http_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "token request ended before its headers arrived");
            request.extend_from_slice(&chunk[..count]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .or_else(|| {
                headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
            })
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while request.len() < header_end + content_length {
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "token request ended before its body arrived");
            request.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(request).unwrap()
    }
    #[tokio::test]
    async fn token_requests_have_a_total_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let server_release = Arc::clone(&release);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{",
                )
                .await
                .unwrap();
            server_release.notified().await;
        });

        let client = oauth_client(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(50),
        )
        .unwrap();
        let mut tokens = OAuthTokens {
            access_token: "expired".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expires_at: None,
            client_id: "client".to_owned(),
            client_secret: None,
            token_url: Url::parse(&format!("http://{address}/token")).unwrap(),
        };
        let error = tokens.refresh_with_client(&client).await.unwrap_err();
        release.notify_one();

        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().to_ascii_lowercase().contains("timed out")),
            "{error:#}"
        );
        assert_eq!(tokens.access_token, "expired");
    }
}
