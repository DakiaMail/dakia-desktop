use crate::storage::MailSummary;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
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
                let response = request
                    .send()
                    .await?
                    .error_for_status()?
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
                let response = self.client.post(base_url.join("api/chat")?).json(&serde_json::json!({
                    "model": model,
                    "stream": false,
                    "messages": [{"role":"system","content":system},{"role":"user","content":user}]
                })).send().await?.error_for_status()?.json::<OllamaResponse>().await?;
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
                    .arg(format!("{system}\n\n{user}"));
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

fn ai_client(
    connect_timeout: Duration,
    request_timeout: Duration,
    read_timeout: Duration,
) -> reqwest::Result<Client> {
    Client::builder()
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
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
    };

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
}
