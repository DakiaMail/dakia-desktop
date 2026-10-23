use super::*;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoreArtifactMessage {
    fixture_id: String,
    message_id: String,
    expected_uid_validity: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoreArtifact {
    schema_version: u32,
    nonce: String,
    database: String,
    messages: Vec<CoreArtifactMessage>,
}

struct UnexpectedClassifier;

impl EmailClassifier for UnexpectedClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("cached MIME artifact projection must not classify or contact a provider")
    }
}

fn request(message_id: &str, expected_uid_validity: i64) -> InvokeRequest {
    InvokeRequest {
        cmd: "message_content".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("valid local URL"),
        body: InvokeBody::Json(json!({
            "messageId": message_id,
            "expectedUidValidity": expected_uid_validity,
        })),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn response_json(body: InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(value) => {
            serde_json::from_str(&value).expect("message_content returns JSON")
        }
        InvokeResponseBody::Raw(_) => panic!("message_content returned an unexpected byte payload"),
    }
}

fn assert_fixture_semantics(fixture_id: &str, message: &MailSummary, content: &Value) {
    let text = content["body_text"].as_str().expect("body_text string");
    let html = content["body_html"].as_str();
    let filenames = content["attachments"]
        .as_array()
        .expect("attachments array")
        .iter()
        .map(|attachment| attachment["filename"].as_str().unwrap())
        .collect::<Vec<_>>();
    match fixture_id {
        "competing-nested-alternatives" => {
            let html = html.expect("nested alternatives keep the selected HTML body");
            assert!(text.contains("Visible plain body."));
            assert!(!text.contains("redacted-notes"));
            assert!(html.contains("Visible <strong>HTML</strong> body."));
            assert_eq!(filenames, ["redacted-notes.txt"]);
        }
        "attached-message-rfc822" => {
            assert_eq!(html, None, "attached message must not become Reader HTML");
            assert!(text.contains("fully redacted"));
            assert!(!text.contains("Original plain text"));
            assert_eq!(filenames, ["forwarded-message.eml"]);
        }
        "multipart-attachment-container" => {
            assert_eq!(html, None, "attached multipart must not become Reader HTML");
            assert!(text.contains("Visible parent body"));
            assert!(!text.contains("Secret child text"));
            assert_eq!(filenames, ["bundle.mime"]);
        }
        "empty-alternative-leaf" => {
            let html = html.expect("later visible HTML survives an empty alternative leaf");
            assert!(text.contains("Visible HTML after an empty plain part."));
            assert!(html.contains("Visible HTML after an empty plain part."));
            assert!(filenames.is_empty());
        }
        "truncated-multipart-lf" => {
            let html = html.expect("selective hydration retains the incomplete HTML leaf");
            assert!(text.contains("first leaf is complete"));
            assert!(html.contains("second leaf and closing boundary are deliberately missing."));
            assert!(!text.contains("--truncated"));
            assert!(!html.contains("--truncated"));
            assert!(filenames.is_empty());
        }
        "charset-matrix" => {
            assert_eq!(html, None, "selected text charset fixture has no HTML body");
            assert!(text.contains("Zażółć"));
            assert!(!text.contains("Unknown charset must not become invented text"));
            assert!(filenames.is_empty());
        }
        "format-flowed-delsp" => {
            let html = html.expect("flowed fixture has a selected HTML alternative");
            assert!(text.contains("soft spacethat continues"));
            assert!(text.contains("> Quoted flowed textcontinues too."));
            assert!(html.contains("This is the HTML alternative."));
            assert!(filenames.is_empty());
        }
        "ambiguous-inline-identities" => {
            let html = html.expect("ambiguous fixture keeps its safe HTML body");
            assert!(html.contains("Fictional resources"));
            assert!(!html.contains("data:image/"));
            assert_eq!(filenames, ["logo.png", "source.pdf"]);
        }
        "international-reader-semantics" => {
            assert_eq!(message.subject, "Fictional 🌍 reader message");
            assert!(text.contains("Tere, maailma 👋jätkub samal real."));
            assert!(text.contains("مرحبا بالعالم"));
            assert!(text.contains("שלום עולם"));
            let html = html.expect("international fixture has an HTML alternative");
            assert!(html.contains("dir=\"rtl\""));
            assert!(html.contains("Fictional signature"));
            assert!(filenames.is_empty());
        }
        "provider-signature-inline" => {
            let html = html.expect("provider signature keeps its selected HTML body");
            assert!(html.contains("Redacted message content for a fictional insurance case."));
            assert!(html.contains("data:image/png;base64,iVBORw0KGgo="));
            assert!(!html.contains("cid:image001.png@redacted"));
            assert_eq!(filenames, ["claim-documents.pdf"]);
            assert_eq!(content["attachments"][0]["is_inline"], false);
            assert_eq!(content["attachments"][0]["is_potentially_unsafe"], false);
        }
        "filename-parameters-and-encodings" => {
            assert_eq!(html, None);
            assert_eq!(text.trim(), "Synthetic filename coverage.");
            assert_eq!(
                filenames,
                [
                    "quarterly report final.pdf",
                    "report-test.txt",
                    "Prügimaja plaan vaated 31.08.pdf",
                    "Prügimaja_31.08.pdf",
                    "Prügimaja.pdf",
                    "invoice;final.pdf",
                    "invoicegpj.exe",
                    "broken%ZZname.pdf",
                ]
            );
            assert!(filenames
                .iter()
                .all(|filename| !filename.contains(['/', '\\', '\u{202e}'])));
            let unsafe_filenames = content["attachments"]
                .as_array()
                .expect("attachments array")
                .iter()
                .filter(|attachment| attachment["is_potentially_unsafe"] == true)
                .map(|attachment| attachment["filename"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(unsafe_filenames, ["invoicegpj.exe"]);
        }
        "linkedin-inline-content-id" => {
            assert_eq!(
                text.trim(),
                "Redacted plain text=content with a foldedline."
            );
            assert!(html
                .expect("LinkedIn fixture keeps its HTML alternative")
                .contains("Redacted HTML=content with a foldedline."));
            assert!(filenames.is_empty());
        }
        "standards-unknown-transfer-encoding" => {
            assert_eq!(text.trim(), "Visible safe body.");
            assert_eq!(html, None);
            assert!(!text.contains("Opaque transport bytes"));
            assert!(!text.contains("<script>"));
            assert_eq!(filenames, ["invoice.exe", "attachment"]);
            assert_eq!(
                content["attachments"][0]["mime_type"],
                "application/octet-stream"
            );
            assert_eq!(
                content["attachments"][1]["mime_type"],
                "application/octet-stream"
            );
            assert_eq!(content["attachments"][0]["size_kind"], "encoded");
            assert_eq!(content["attachments"][1]["size_kind"], "encoded");
            assert_eq!(content["attachments"][0]["is_potentially_unsafe"], true);
            assert_eq!(content["attachments"][1]["is_potentially_unsafe"], false);
        }
        other => panic!("unexpected MIME artifact fixture: {other}"),
    }
}

#[test]
fn real_mime_profile_reaches_reader_through_the_message_content_tauri_command() {
    let Some(root) = std::env::var_os("DAKIA_MIME_READER_CORE_ARTIFACT_ROOT") else {
        // The orchestrator supplies a real profile exported by dakia-core. A
        // normal workspace run still compiles this production-boundary test;
        // scripts/test-mime-reader-artifact.mjs is the evidence lane.
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("core artifact root exists");
    let artifact: CoreArtifact = serde_json::from_slice(
        &std::fs::read(root.join("core-manifest.json")).expect("read core artifact manifest"),
    )
    .expect("decode core artifact manifest");
    let nonce = std::env::var("DAKIA_MIME_READER_ARTIFACT_NONCE")
        .expect("orchestrated artifact proof supplies a nonce");
    assert_eq!(artifact.schema_version, 1);
    assert_eq!(artifact.nonce, nonce);
    assert_eq!(artifact.messages.len(), 13);

    let store = tauri::async_runtime::block_on(Store::open(root.join(&artifact.database)))
        .expect("open the core-generated profile through production Store");
    let state = Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store: store.clone(),
        data_dir: root.clone(),
        classifier: Mutex::new(Box::new(UnexpectedClassifier)),
        classification_owner: "mime-reader-artifact-test".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        translation_downloads: Mutex::new(HashMap::new()),
    });
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![message_content])
        .build(super::test_support::mock_context())
        .expect("build mock Tauri runtime");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-13131313131343138313131313131313",
        WebviewUrl::default(),
    )
    .build()
    .expect("build capability-scoped Reader window");

    let mut messages = Vec::new();
    for expected in artifact.messages {
        let message = tauri::async_runtime::block_on(store.message(&expected.message_id))
            .expect("read parsed message")
            .expect("core profile retained parsed message");
        let body = get_ipc_response(
            &reader,
            request(&expected.message_id, expected.expected_uid_validity),
        )
        .unwrap_or_else(|error| panic!("message_content IPC rejected: {error}"));
        let content = response_json(body);
        assert_fixture_semantics(&expected.fixture_id, &message, &content);
        messages.push(json!({
            "fixtureId": expected.fixture_id,
            "message": message,
            "content": content,
        }));
    }

    if let Some(path) = std::env::var_os("DAKIA_MIME_READER_TAURI_ARTIFACT") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "nonce": nonce,
                "messages": messages,
            }))
            .unwrap(),
        )
        .expect("write actual Tauri command artifact");
    }
}
