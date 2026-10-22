//! Native-command privacy boundaries for hidden AI features.
//!
//! AI controls are intentionally absent from the shipped UI. These tests use
//! Tauri's normal `main` window capability and a fictional local endpoint to
//! prove that forged IPC cannot open a configured endpoint or a local command.

use super::*;
use chrono::{TimeZone, Utc};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::Duration,
};
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

const CURRENT_EPOCH: i64 = 77;

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("hidden AI command tests must not classify mail")
    }
}

fn test_account() -> Account {
    AccountDraft {
        email: "reader@example.test".into(),
        display_name: "AI privacy test".into(),
        provider_id: Some("custom".into()),
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
    .into_account(provider::by_id("custom").expect("custom provider"))
}

fn private_message(account_id: Uuid) -> MailSummary {
    MailSummary {
        id: "private-local-row-id".into(),
        account_id: account_id.to_string(),
        mailbox: "Private Folder".into(),
        uid: 42,
        mailbox_uid_validity: None,
        message_id: None,
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "private-thread-id".into(),
        subject: "Selected quarterly plan".into(),
        from_name: Some("Mara".into()),
        from_address: "mara@example.test".into(),
        to_addresses: "private-recipient@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap(),
        snippet: "private-preview-only".into(),
        body_text: "The selected body text.".into(),
        body_html: None,
        content_state: "complete".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: false,
        is_flagged: false,
        has_attachments: false,
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: Vec::new(),
    }
}

async fn state_with_message() -> (Arc<AppState>, MailSummary) {
    let store = Store::in_memory().await.expect("in-memory store");
    let account = test_account();
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(
            account.id,
            "Private Folder",
            "Private Folder",
            CURRENT_EPOCH as u32,
            1,
            true,
        )
        .await
        .expect("save mailbox epoch");
    let message = private_message(account.id);
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("save message");
    (
        Arc::new(AppState {
            realtime: RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: PathBuf::new(),
            classifier: Mutex::new(Box::new(NeverClassifier)),
            classification_owner: "hidden-ai-command-test".into(),
            classification: Arc::new(ClassificationScheduler::default()),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: MailRebuildCancellations::default(),
            account_operations: AccountOperationLocks::default(),
            translation_downloads: Mutex::new(HashMap::new()),
        }),
        message,
    )
}

async fn app() -> (
    tauri::App<tauri::test::MockRuntime>,
    Arc<AppState>,
    MailSummary,
) {
    let (state, message) = state_with_message().await;
    let app_state = state.clone();
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ai_draft,
            ai_available,
            ai_summarize,
            set_ai_api_key
        ])
        .build(super::test_support::mock_context())
        .expect("build mock Tauri app");
    (app, app_state, message)
}

fn request(command: &str, input: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: command.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("valid local URL"),
        body: InvokeBody::Json(serde_json::json!({ "input": input })),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn remote_input(base_url: String, message: &MailSummary) -> serde_json::Value {
    serde_json::json!({
        "provider": "openai",
        "baseUrl": base_url,
        "model": "fictional-privacy-model",
        "apiKey": "fictional-api-key",
        "messageIds": [message.id],
        "messageTargets": [{
            "messageId": message.id,
            "expectedUidValidity": CURRENT_EPOCH,
        }],
        "instruction": "Reply with the approved date.",
    })
}

fn remote_input_without_api_key(base_url: String, message: &MailSummary) -> serde_json::Value {
    let mut input = remote_input(base_url, message);
    input
        .as_object_mut()
        .expect("AI input fixture is an object")
        .remove("apiKey");
    input
}

fn request_capture_server() -> (String, Receiver<bool>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fictional AI service");
    let address = listener.local_addr().expect("fictional service address");
    listener
        .set_nonblocking(true)
        .expect("make fictional service nonblocking");
    let (sent, received) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buffer = [0_u8; 1024];
                    let _ = stream.read(&mut buffer);
                    sent.send(true).expect("publish received request");
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        sent.send(false).expect("publish no request");
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept fictional AI request: {error}"),
            }
        }
    });
    (format!("http://{address}/v1/"), received, server)
}

fn forbidden(response: Result<tauri::ipc::InvokeResponseBody, serde_json::Value>) {
    let detail = response
        .expect_err("hidden AI commands must reject native IPC")
        .to_string();
    assert!(
        detail.contains("not allowed"),
        "normal windows must not retain hidden AI command permission: {detail}"
    );
}

#[test]
fn hidden_ai_feature_guard_rejects_before_any_provider_configuration() {
    assert_eq!(
        require_ai_features_enabled().unwrap_err(),
        AI_FEATURES_DISABLED_ERROR
    );
}

#[test]
fn hidden_ai_draft_is_forbidden_before_opening_a_configured_endpoint() {
    let (app, _state, message) = tauri::async_runtime::block_on(app());
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let (base_url, captured, server) = request_capture_server();

    forbidden(get_ipc_response(
        &main,
        request("ai_draft", remote_input(base_url, &message)),
    ));
    assert!(
        !captured
            .recv_timeout(Duration::from_secs(3))
            .expect("fictional endpoint must report whether it was reached"),
        "a forged hidden AI draft must not send mail context"
    );
    server.join().expect("fictional service completes");
}

#[test]
fn hidden_ai_availability_is_forbidden_before_opening_a_configured_endpoint() {
    let (app, _state, message) = tauri::async_runtime::block_on(app());
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let (base_url, captured, server) = request_capture_server();

    forbidden(get_ipc_response(
        &main,
        request("ai_available", remote_input(base_url, &message)),
    ));
    assert!(
        !captured
            .recv_timeout(Duration::from_secs(3))
            .expect("fictional endpoint must report whether it was reached"),
        "availability probing must not send a stored or supplied credential to a caller URL"
    );
    server.join().expect("fictional service completes");
}

#[test]
fn hidden_ai_availability_cannot_send_a_fictional_stored_key_to_a_caller_endpoint() {
    let (app, state, message) = tauri::async_runtime::block_on(app());
    tauri::async_runtime::block_on(
        state
            .store
            .set_secret("dev.dakia.mail:ai:api-key", "fictional-stored-ai-key"),
    )
    .expect("store fictional key");
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let (base_url, captured, server) = request_capture_server();

    forbidden(get_ipc_response(
        &main,
        request(
            "ai_available",
            remote_input_without_api_key(base_url, &message),
        ),
    ));
    assert!(
        !captured
            .recv_timeout(Duration::from_secs(3))
            .expect("fictional endpoint must report whether it was reached"),
        "a forged hidden availability probe must not send the stored key to its caller-selected endpoint"
    );
    server.join().expect("fictional service completes");
}

#[cfg(unix)]
#[test]
fn hidden_ai_draft_is_forbidden_before_launching_a_configured_local_executable() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().expect("create local command fixture");
    let executable = fixture.path().join("must-not-launch.sh");
    let marker = fixture.path().join("process-was-launched");
    fs::write(&executable, "#!/bin/sh\ntouch \"$2\"\n").expect("write local command fixture");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make local command fixture executable");

    let (app, _state, message) = tauri::async_runtime::block_on(app());
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let input = serde_json::json!({
        "provider": "local",
        "model": "fictional-local-model",
        "executable": executable,
        "modelPath": marker,
        "messageIds": [message.id],
        "messageTargets": [{
            "messageId": message.id,
            "expectedUidValidity": CURRENT_EPOCH,
        }],
        "instruction": "This process must never launch.",
    });

    forbidden(get_ipc_response(&main, request("ai_draft", input)));
    assert!(
        !marker.exists(),
        "a forged hidden AI command must not launch its caller-selected executable"
    );
}
