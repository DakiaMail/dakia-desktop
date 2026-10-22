use crate::storage::MailSummary;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{path::PathBuf, process::Stdio};
use tokio::process::Command;
use url::Url;

const AI_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const AI_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AiProvider {
    OpenAiCompatible {
        base_url: Url,
        model: String,
    },
    LocalCommand {
        executable: PathBuf,
        model_path: PathBuf,
        extra_args: Vec<String>,
    },
    Ollama {
        base_url: Url,
        model: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: AiProvider,
    #[serde(skip_serializing, default)]
    pub api_key: Option<SecretString>,
}

pub struct AiService {
    config: AiConfig,
    client: Client,
}

impl AiService {
    pub fn new(config: AiConfig) -> Self {
        Self {
            config,
            client: ai_client(AI_CONNECT_TIMEOUT, AI_REQUEST_TIMEOUT, AI_READ_TIMEOUT)
                .expect("the static AI HTTP client configuration is valid"),
        }
    }

    pub async fn is_available(&self) -> bool {
        match &self.config.provider {
            AiProvider::OpenAiCompatible { base_url, .. } => {
                let Ok(endpoint) = base_url.join("models") else {
                    return false;
                };
                let mut request = self.client.get(endpoint);
                if let Some(key) = &self.config.api_key {
                    request = request.bearer_auth(key.expose_secret());
                }
                matches!(
                    tokio::time::timeout(Duration::from_secs(3), request.send()).await,
                    Ok(Ok(response)) if response.status().is_success()
                )
            }
            AiProvider::Ollama { base_url, .. } => {
                let Ok(endpoint) = base_url.join("api/tags") else {
                    return false;
                };
                matches!(
                    tokio::time::timeout(Duration::from_secs(2), self.client.get(endpoint).send()).await,
                    Ok(Ok(response)) if response.status().is_success()
                )
            }
            AiProvider::LocalCommand {
                executable,
                model_path,
                ..
            } => executable.is_file() && model_path.is_file(),
        }
    }

    pub async fn summarize(&self, messages: &[MailSummary]) -> Result<String> {
        let content = format_messages(messages);
        self.complete("Summarize these emails. Identify decisions, requests, deadlines, and unresolved questions. Keep names and dates precise.", &content).await
    }

    pub async fn draft(&self, instruction: &str, context: &[MailSummary]) -> Result<String> {
        let content = format!(
            "Instruction:\n{instruction}\n\nEmail context:\n{}",
            format_messages(context)
        );
        self.complete("Write a concise email draft. Return only the draft body. Do not invent commitments or facts.", &content).await
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        match &self.config.provider {
            AiProvider::OpenAiCompatible { base_url, model } => {
                let endpoint = base_url.join("chat/completions")?;
                let mut request = self.client.post(endpoint).json(&serde_json::json!({
                    "model": model,
                    "messages": [{"role":"system","content":system},{"role":"user","content":user}],
                    "temperature": 0.2
                }));
                if let Some(key) = &self.config.api_key {
                    request = request.bearer_auth(key.expose_secret());
                }
                let response = require_ai_success(request.send().await?)?
                    .json::<ChatResponse>()
                    .await?;
                response
                    .choices
                    .into_iter()
                    .next()
                    .map(|choice| choice.message.content)
                    .context("AI provider returned no completion")
            }
            AiProvider::Ollama { base_url, model } => {
                let response = require_ai_success(
                    self.client
                        .post(base_url.join("api/chat")?)
                        .json(&serde_json::json!({
                            "model": model,
                            "stream": false,
                            "messages": [{"role":"system","content":system},{"role":"user","content":user}]
                        }))
                        .send()
                        .await?,
                )?
                .json::<OllamaResponse>()
                .await?;
                Ok(response.message.content)
            }
            AiProvider::LocalCommand {
                executable,
                model_path,
                extra_args,
            } => {
                let mut command = Command::new(executable);
                command
                    .arg("-m")
                    .arg(model_path)
                    .args(extra_args)
                    .arg("--prompt")
                    .arg(format!("{system}\n\n{user}"))
                    // A local provider receives the prompt as an argument. It must
                    // never inherit Dakia's stdin, which may be an interactive
                    // terminal or a caller-owned pipe containing unrelated input.
                    .stdin(Stdio::null());
                let output = command
                    .output()
                    .await
                    .context("failed to start local AI command")?;
                if !output.status.success() {
                    bail!(
                        "local AI command failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Ok(String::from_utf8(output.stdout)?.trim().to_owned())
            }
        }
    }
}

fn require_ai_success(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        bail!("AI provider returned HTTP status {status}")
    }
}

fn ai_client(
    connect_timeout: Duration,
    request_timeout: Duration,
    read_timeout: Duration,
) -> reqwest::Result<Client> {
    Client::builder()
        // Mail context and provider credentials must never be forwarded to a
        // redirect target selected by a provider response.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .read_timeout(read_timeout)
        .build()
}

fn format_messages(messages: &[MailSummary]) -> String {
    messages
        .iter()
        .map(|message| {
            format!(
                "From: {} <{}>\nSubject: {}\nDate: {}\n\n{}",
                message.from_name.as_deref().unwrap_or(""),
                message.from_address,
                message.subject,
                message.received_at.to_rfc3339(),
                message.body_text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}
#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}
#[derive(Deserialize)]
struct OllamaResponse {
    message: ChatMessage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, Notify},
    };

    async fn capture_json_server(
        status: &str,
        response_body: &str,
    ) -> (Url, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();
        let status = status.to_owned();
        let response_body = response_body.to_owned();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let _ = request_tx.send(request);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (
            Url::parse(&format!("http://{address}/v1/")).unwrap(),
            request_rx,
        )
    }

    async fn capture_redirect_server() -> (Url, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "client closed before its request completed");
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let _ = request_tx.send(request);
            socket
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://unresolvable-ai-redirect.invalid/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        (
            Url::parse(&format!("http://{address}/v1/")).unwrap(),
            request_rx,
        )
    }

    fn private_message() -> MailSummary {
        MailSummary {
            id: "private-row-id".into(),
            account_id: "private-account-id".into(),
            mailbox: "Private Folder".into(),
            uid: 987,
            mailbox_uid_validity: None,
            message_id: Some("private-message-id@example.test".into()),
            gmail_message_id: None,
            in_reply_to: Some("private-parent-id@example.test".into()),
            reference_ids: Some("private-reference-id@example.test".into()),
            thread_id: "private-thread-id".into(),
            subject: "Selected quarterly plan".into(),
            from_name: Some("Mara".into()),
            from_address: "mara@example.test".into(),
            to_addresses: "private-recipient@example.test".into(),
            cc_addresses: "private-cc@example.test".into(),
            bcc_addresses: "private-bcc@example.test".into(),
            reply_to_addresses: "private-reply@example.test".into(),
            received_at: Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap(),
            snippet: "private-preview-only".into(),
            body_text: "The selected body text.".into(),
            body_html: Some("<p>private-html-only</p>".into()),
            content_state: "complete".into(),
            unsubscribe_kind: Some("web".into()),
            unsubscribe_url: Some("https://private-unsubscribe.example.test".into()),
            is_read: false,
            is_flagged: true,
            has_attachments: true,
            category: Some("updates".into()),
            classification_confidence: Some(0.99),
            classification_source: Some("model".into()),
            classification_signals: "private-classifier-signal".into(),
            attachments: Vec::new(),
        }
    }

    async fn stalled_json_server() -> (Url, Arc<Notify>) {
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
        (Url::parse(&format!("http://{address}/")).unwrap(), release)
    }

    #[tokio::test]
    async fn completion_has_a_total_request_deadline() {
        let (base_url, release) = stalled_json_server().await;
        let service = AiService {
            config: AiConfig {
                provider: AiProvider::OpenAiCompatible {
                    base_url,
                    model: "test".to_owned(),
                },
                api_key: None,
            },
            client: ai_client(
                Duration::from_millis(100),
                Duration::from_millis(100),
                Duration::from_millis(50),
            )
            .unwrap(),
        };

        let error = service.complete("system", "user").await.unwrap_err();
        release.notify_one();

        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().to_ascii_lowercase().contains("timed out")),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn openai_request_sends_only_the_explicit_email_context_and_auth_header() {
        let (base_url, captured) = capture_json_server(
            "200 OK",
            r#"{"choices":[{"message":{"content":"Draft result"}}]}"#,
        )
        .await;
        let service = AiService::new(AiConfig {
            provider: AiProvider::OpenAiCompatible {
                base_url,
                model: "privacy-test-model".to_owned(),
            },
            api_key: Some(SecretString::from("secret-api-key")),
        });

        let result = service
            .draft("Reply with the approved date.", &[private_message()])
            .await
            .unwrap();
        assert_eq!(result, "Draft result");

        let request = captured.await.unwrap();
        let header_end = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let body: serde_json::Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-api-key"));
        assert_eq!(body["model"], "privacy-test-model");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);

        let user_context = body["messages"][1]["content"].as_str().unwrap();
        for included in [
            "Reply with the approved date.",
            "Selected quarterly plan",
            "Mara <mara@example.test>",
            "The selected body text.",
            "2026-08-26T12:00:00+00:00",
        ] {
            assert!(
                user_context.contains(included),
                "missing selected context: {included}"
            );
        }
        for private_metadata in [
            "private-row-id",
            "private-account-id",
            "Private Folder",
            "private-message-id",
            "private-recipient",
            "private-cc",
            "private-bcc",
            "private-reply",
            "private-preview-only",
            "private-html-only",
            "private-unsubscribe",
            "private-classifier-signal",
        ] {
            assert!(
                !user_context.contains(private_metadata),
                "leaked storage or message metadata: {private_metadata}"
            );
        }
    }

    #[tokio::test]
    async fn provider_redirect_cannot_forward_mail_context_or_credentials() {
        let (base_url, captured) = capture_redirect_server().await;
        let service = AiService::new(AiConfig {
            provider: AiProvider::OpenAiCompatible {
                base_url,
                model: "privacy-test-model".to_owned(),
            },
            api_key: Some(SecretString::from("secret-api-key")),
        });

        let error = service
            .draft("Reply briefly.", &[private_message()])
            .await
            .unwrap_err();
        let request = captured.await.unwrap();
        assert!(
            error.to_string().contains("307 Temporary Redirect"),
            "redirect following would replace the provider response with a request to its untrusted Location: {error:#}"
        );
        let serialized = String::from_utf8(request).unwrap();
        assert!(
            serialized.contains("authorization: Bearer secret-api-key")
                || serialized.contains("Authorization: Bearer secret-api-key")
        );
        assert!(serialized.contains("The selected body text."));
    }

    #[tokio::test]
    async fn provider_errors_do_not_expose_the_api_key_or_email_context() {
        let (base_url, captured) = capture_json_server(
            "500 Internal Server Error",
            r#"{"error":"provider unavailable"}"#,
        )
        .await;
        let service = AiService::new(AiConfig {
            provider: AiProvider::OpenAiCompatible {
                base_url,
                model: "privacy-test-model".to_owned(),
            },
            api_key: Some(SecretString::from("secret-api-key")),
        });

        let error = service.summarize(&[private_message()]).await.unwrap_err();
        let _ = captured.await.unwrap();
        let rendered = format!("{error:#}");

        assert!(rendered.contains("500 Internal Server Error"), "{rendered}");
        for private_value in [
            "secret-api-key",
            "Selected quarterly plan",
            "mara@example.test",
            "The selected body text.",
        ] {
            assert!(
                !rendered.contains(private_value),
                "provider error leaked private value {private_value}: {rendered}"
            );
        }
    }
}
