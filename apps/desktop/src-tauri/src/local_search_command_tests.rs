//! Production Tauri dispatcher acceptance for account-scoped local search.

use super::*;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("local search must not invoke classification")
    }
}

fn account(id: Uuid, email: &str) -> Account {
    let mut account = AccountDraft {
        email: email.into(),
        display_name: "Local search command fixture".into(),
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
    .into_account(provider::by_id("custom").expect("custom provider"));
    account.id = id;
    account
}

fn message(
    id: &str,
    account_id: Uuid,
    mailbox: &str,
    uid: i64,
    received_at: DateTime<Utc>,
) -> MailSummary {
    MailSummary {
        id: id.into(),
        account_id: account_id.to_string(),
        mailbox: mailbox.into(),
        uid,
        mailbox_uid_validity: Some(77),
        message_id: Some(format!("<{id}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: id.into(),
        subject: format!("雪 töö tag {id}"),
        from_name: Some("Fictional Search Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at,
        snippet: "Unicode command result".into(),
        body_text: String::new(),
        body_html: None,
        content_state: "catalogue".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: false,
        is_flagged: false,
        has_attachments: false,
        category: Some("transactions".into()),
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: Vec::new(),
    }
}

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../testdata/tauri-contracts/local-search.json"
    ))
    .expect("parse shared local search fixture")
}

fn state(store: Store) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir: PathBuf::new(),
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "local-search-command-test".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn request(body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: "search".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("valid local URL"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn response_page(body: InvokeResponseBody) -> MailConversationPage {
    match body {
        InvokeResponseBody::Json(value) => {
            serde_json::from_str(&value).expect("deserialize local search IPC response")
        }
        InvokeResponseBody::Raw(_) => panic!("local search must return JSON"),
    }
}

#[test]
fn search_command_dispatches_shared_query_with_scope_cursor_and_limit_clamp() {
    let fixture = fixture();
    let query: SearchQuery =
        serde_json::from_value(fixture["arguments"]["query"].clone()).expect("fixture query");
    let selected_id = query.account_ids[0];
    let other_id = Uuid::new_v4();
    let store = tauri::async_runtime::block_on(Store::in_memory()).expect("in-memory store");
    tauri::async_runtime::block_on(async {
        store
            .save_account(&account(selected_id, "selected@example.test"))
            .await
            .expect("save selected account");
        store
            .save_account(&account(other_id, "other@example.test"))
            .await
            .expect("save other account");
        let timestamp: DateTime<Utc> = "2026-10-21T08:00:00Z".parse().unwrap();
        let mut messages = (0..502)
            .map(|index| {
                message(
                    &format!("selected-{index:03}"),
                    selected_id,
                    "Archive/Arved",
                    i64::from(index + 1),
                    timestamp - ChronoDuration::seconds(i64::from(index)),
                )
            })
            .collect::<Vec<_>>();
        let mut wrong_text = message(
            "wrong-text",
            selected_id,
            "Archive/Arved",
            600,
            timestamp - ChronoDuration::milliseconds(100),
        );
        wrong_text.subject = "ordinary nonmatching subject".into();
        let mut already_read = message(
            "already-read",
            selected_id,
            "Archive/Arved",
            601,
            timestamp - ChronoDuration::milliseconds(200),
        );
        already_read.is_read = true;
        let mut flagged = message(
            "flagged",
            selected_id,
            "Archive/Arved",
            602,
            timestamp - ChronoDuration::milliseconds(300),
        );
        flagged.is_flagged = true;
        let mut wrong_category = message(
            "wrong-category",
            selected_id,
            "Archive/Arved",
            603,
            timestamp - ChronoDuration::milliseconds(400),
        );
        wrong_category.category = Some("people".into());
        messages.extend([
            wrong_text,
            already_read,
            flagged,
            wrong_category,
            message(
                "other-account",
                other_id,
                "Archive/Arved",
                1,
                timestamp - ChronoDuration::milliseconds(500),
            ),
            message(
                "wrong-mailbox",
                selected_id,
                "INBOX",
                503,
                timestamp - ChronoDuration::milliseconds(600),
            ),
        ]);
        store
            .upsert_messages(&messages)
            .await
            .expect("seed local search catalogue");
    });

    let app = mock_builder()
        .manage(state(store))
        .invoke_handler(tauri::generate_handler![search])
        .build(super::test_support::mock_context())
        .expect("build production search dispatcher");
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");

    let exact = response_page(
        get_ipc_response(&main, request(fixture["arguments"].clone()))
            .expect("shared local search command succeeds"),
    );
    assert_eq!(exact.conversations.len(), 500);
    assert!(exact.next_cursor.is_some());
    assert_eq!(exact.conversations[0].latest.id, "selected-001");
    assert_eq!(exact.conversations[499].latest.id, "selected-500");
    assert!(exact.conversations.iter().all(|conversation| {
        conversation.account_id == selected_id.to_string()
            && conversation.latest.mailbox == "Archive/Arved"
    }));
    assert!(exact
        .conversations
        .iter()
        .all(|conversation| conversation.latest.id != "wrong-mailbox"));
    let excluded = [
        "wrong-text",
        "already-read",
        "flagged",
        "wrong-category",
        "other-account",
        "wrong-mailbox",
        "selected-000",
    ];
    assert!(exact
        .conversations
        .iter()
        .all(|conversation| !excluded.contains(&conversation.latest.id.as_str())));
    let mut continuation = fixture["arguments"].clone();
    continuation["query"]["cursor"] =
        serde_json::to_value(exact.next_cursor.as_ref().expect("continuation cursor"))
            .expect("serialize continuation cursor");
    let continued = response_page(
        get_ipc_response(&main, request(continuation)).expect("continued local search succeeds"),
    );
    assert_eq!(
        continued
            .conversations
            .iter()
            .map(|conversation| conversation.latest.id.as_str())
            .collect::<Vec<_>>(),
        ["selected-501"]
    );
    assert!(continued.next_cursor.is_none());
    assert!(continued.conversations.iter().all(|conversation| {
        conversation.account_id == selected_id.to_string()
            && conversation.latest.mailbox == "Archive/Arved"
    }));

    let mut oversized = fixture["arguments"].clone();
    oversized["query"]["limit"] = serde_json::json!(900);
    let clamped = response_page(
        get_ipc_response(&main, request(oversized)).expect("oversized local search succeeds"),
    );
    assert_eq!(clamped.conversations.len(), 500);
    assert!(clamped.next_cursor.is_some());

    let reader =
        WebviewWindowBuilder::new(&app, "reader-local-search-denied", WebviewUrl::default())
            .build()
            .expect("reader window");
    let denial = get_ipc_response(&reader, request(fixture["arguments"].clone()))
        .expect_err("Reader must not search whole-mailbox metadata")
        .to_string();
    assert!(
        denial.contains("not allowed"),
        "unexpected denial: {denial}"
    );
}
