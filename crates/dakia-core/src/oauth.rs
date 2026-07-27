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
        let mut bytes = vec![0u8; 8192];
        let count = timeout(std::time::Duration::from_secs(10), socket.read(&mut bytes)).await??;
        let request = std::str::from_utf8(&bytes[..count])?;
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
            bail!("OAuth provider denied access: {message}");
        }
        let code = params
            .get("code")
            .context("OAuth callback did not include an authorization code")?;
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await?;
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
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
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
