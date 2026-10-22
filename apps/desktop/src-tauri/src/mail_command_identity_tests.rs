//! Dispatcher-level identity checks for every user-facing message mutation.
//! The production Tauri command wrapper, capability ACL, argument decoder,
//! account operation lock, and immutable target validation all participate.

use super::*;
use chrono::Utc;
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

const CURRENT_EPOCH: i64 = 77;
const CHANGED_MESSAGE: &str = "This message changed. Refresh the mailbox before trying again.";

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("target validation must run before classification")
    }
}

fn test_account(email: &str) -> Account {
    AccountDraft {
        email: email.into(),
        display_name: "Command identity test".into(),
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

fn message(account_id: Uuid) -> MailSummary {
    MailSummary {
        id: format!("{account_id}:INBOX:42"),
        account_id: account_id.to_string(),
        mailbox: "INBOX".into(),
        uid: 42,
        mailbox_uid_validity: None,
        message_id: Some("<command-identity@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "command-identity-thread".into(),
        subject: "Immutable target".into(),
        from_name: None,
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: String::new(),
        body_text: String::new(),
        body_html: None,
        content_state: "headers_only".into(),
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

fn app() -> (
    tauri::App<tauri::test::MockRuntime>,
    Account,
    Account,
    MailSummary,
) {
    let store = tauri::async_runtime::block_on(Store::in_memory()).expect("in-memory store");
    let owner = test_account("owner@example.test");
    let other = test_account("other@example.test");
    let current = tauri::async_runtime::block_on(async {
        store.save_account(&owner).await.expect("save owner");
        store
            .save_account(&other)
            .await
            .expect("save other account");
        store
            .save_mailbox_catalog_state(owner.id, "INBOX", "INBOX", CURRENT_EPOCH as u32, 1, true)
            .await
            .expect("save mailbox epoch");
        let message = message(owner.id);
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .expect("save message");
        store
            .message(&message.id)
            .await
            .expect("read message")
            .expect("message exists")
    });
    let state = Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir: PathBuf::new(),
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "mail-command-identity-test".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    });
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            set_message_category,
            set_message_read,
            set_message_starred,
            apply_mailbox_action,
        ])
        .build(super::test_support::mock_context())
        .expect("build mock Tauri app");
    (app, owner, other, current)
}

fn request(command: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: command.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("valid local URL"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn rejection(response: Result<tauri::ipc::InvokeResponseBody, serde_json::Value>) -> String {
    response
        .expect_err("the immutable target must be rejected")
        .to_string()
}

fn assert_identity_rejection(detail: &str, command: &str) {
    assert!(
        detail.contains(CHANGED_MESSAGE),
        "{command} must reach production target validation after ACL and decoding: {detail}"
    );
    assert!(
        !detail.contains("not allowed"),
        "{command} was rejected by ACL instead of exercising identity validation: {detail}"
    );
}

#[test]
fn read_star_and_category_commands_reject_stale_epochs_through_tauri_dispatch() {
    let (app, _owner, _other, current) = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let stale_epoch = CURRENT_EPOCH - 1;
    let cases = [
        (
            "set_message_read",
            serde_json::json!({
                "messageId": current.id,
                "read": true,
                "expectedUidValidity": stale_epoch,
            }),
        ),
        (
            "set_message_starred",
            serde_json::json!({
                "messageId": current.id,
                "starred": true,
                "expectedUidValidity": stale_epoch,
            }),
        ),
        (
            "set_message_category",
            serde_json::json!({
                "messageId": current.id,
                "category": "people",
                "expectedUidValidity": stale_epoch,
            }),
        ),
    ];
    for (command, body) in cases {
        let detail = rejection(get_ipc_response(&main, request(command, body)));
        assert_identity_rejection(&detail, command);
    }
}

#[test]
fn every_move_delete_and_restore_command_rejects_a_cross_account_target_through_tauri_dispatch() {
    let (app, _owner, other, current) = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    for action in ["archive", "spam", "not_spam", "trash", "delete"] {
        let detail = rejection(get_ipc_response(
            &main,
            request(
                "apply_mailbox_action",
                serde_json::json!({
                    "accountId": other.id,
                    "mailbox": current.mailbox,
                    "uid": current.uid,
                    "action": action,
                    "messageId": current.id,
                    "expectedUidValidity": CURRENT_EPOCH,
                }),
            ),
        ));
        assert_identity_rejection(&detail, action);
    }
}

#[test]
fn every_move_delete_and_restore_command_rejects_each_wrong_locator_field() {
    let (app, owner, _other, current) = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    for action in ["archive", "spam", "not_spam", "trash", "delete"] {
        for (case, mailbox, uid, message_id, expected_uid_validity) in [
            (
                "mailbox",
                "Archive",
                current.uid,
                Some(current.id.as_str()),
                CURRENT_EPOCH,
            ),
            (
                "uid",
                current.mailbox.as_str(),
                current.uid + 1,
                Some(current.id.as_str()),
                CURRENT_EPOCH,
            ),
            (
                "message id",
                current.mailbox.as_str(),
                current.uid,
                Some("other-id"),
                CURRENT_EPOCH,
            ),
            (
                "missing message id",
                current.mailbox.as_str(),
                current.uid,
                None,
                CURRENT_EPOCH,
            ),
            (
                "UIDVALIDITY",
                current.mailbox.as_str(),
                current.uid,
                Some(current.id.as_str()),
                CURRENT_EPOCH - 1,
            ),
        ] {
            let detail = rejection(get_ipc_response(
                &main,
                request(
                    "apply_mailbox_action",
                    serde_json::json!({
                        "accountId": owner.id,
                        "mailbox": mailbox,
                        "uid": uid,
                        "action": action,
                        "messageId": message_id,
                        "expectedUidValidity": expected_uid_validity,
                    }),
                ),
            ));
            assert_identity_rejection(&detail, &format!("{action} with wrong {case}"));
        }
    }
}

#[test]
fn complete_move_delete_action_inventory_accepts_the_exact_locator_before_remote_work() {
    for action in ["archive", "spam", "not_spam", "trash", "delete"] {
        let (app, owner, _other, current) = app();
        let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
            .build()
            .expect("main window");
        let detail = rejection(get_ipc_response(
            &main,
            request(
                "apply_mailbox_action",
                serde_json::json!({
                    "accountId": owner.id,
                    "mailbox": current.mailbox,
                    "uid": current.uid,
                    "action": action,
                    "messageId": current.id,
                    "expectedUidValidity": CURRENT_EPOCH,
                }),
            ),
        ));
        assert!(
            !detail.contains(CHANGED_MESSAGE) && !detail.contains("not allowed"),
            "the exact {action} locator must pass ACL, decoding, and identity validation before the fictional account's missing credentials stop remote work: {detail}"
        );
    }
}

#[test]
fn repeated_batch_dispatch_keeps_each_item_bound_to_its_own_locator() {
    let (app, owner, _other, current) = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let batch = [
        ("archive", current.id.clone(), current.uid + 1),
        ("spam", "other-id".into(), current.uid),
        ("not_spam", current.id.clone(), current.uid + 1),
        ("trash", "other-id".into(), current.uid),
        ("delete", current.id.clone(), current.uid + 1),
    ];
    for (action, message_id, uid) in batch {
        let detail = rejection(get_ipc_response(
            &main,
            request(
                "apply_mailbox_action",
                serde_json::json!({
                    "accountId": owner.id,
                    "mailbox": current.mailbox,
                    "uid": uid,
                    "action": action,
                    "messageId": message_id,
                    "expectedUidValidity": CURRENT_EPOCH,
                }),
            ),
        ));
        assert_identity_rejection(&detail, &format!("batch {action} item"));
    }
}
