//! Production Tauri mutation commands backed by a fictional TLS IMAP server.
//!
//! The accompanying runner creates the explicitly opt-in, sentinel-marked
//! native-mail-safety profile and loopback CA.  This test dispatches the real
//! command names and JSON argument decoder, then lets `MailService` perform
//! the provider operation.  The transcript server rejects any unexpected
//! command, so a green result includes the actual wire shape.

use super::*;
use chrono::Utc;
use serde_json::{json, Value};
use std::{env, fs};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

const EPOCH: i64 = 77;
const STALE_EPOCH: i64 = 76;

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("mail action command acceptance must not run classification")
    }
}

fn request(command: &str, body: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: command.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("local Tauri URL"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn app_state(store: Store, data_dir: PathBuf) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir,
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "mail-command-tls-artifact".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn summary(account: &Account, mailbox: &str, uid: u32) -> MailSummary {
    MailSummary {
        id: format!("{}:{mailbox}:{uid}", account.id),
        account_id: account.id.to_string(),
        mailbox: mailbox.into(),
        uid: i64::from(uid),
        mailbox_uid_validity: None,
        message_id: Some(format!("<mail-command-{uid}@localhost>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("mail-command-thread-{uid}"),
        subject: format!("Fictional command target {uid}"),
        from_name: Some("Fictional Provider".into()),
        from_address: "provider@localhost".into(),
        to_addresses: account.email.clone(),
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

fn recorded_session_count(path: &Path) -> usize {
    let events: Vec<serde_json::Value> = fs::read_to_string(path)
        .expect("read complete fictional IMAP transcript")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("valid transcript JSON"))
        .collect();
    events
        .iter()
        .filter(|event| event["event"] == "opened")
        .count()
}

fn response_json(body: InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(value) => serde_json::from_str(&value).expect("JSON IPC body"),
        InvokeResponseBody::Raw(_) => panic!("mail mutation command returned raw IPC bytes"),
    }
}

#[test]
fn production_tauri_mail_actions_use_only_the_exact_fictional_tls_locator() {
    let Some(root) = env::var_os("DAKIA_MAIL_COMMAND_TLS_ROOT") else {
        // The regular source suite compiles this module.  Its runner supplies
        // the isolated profile, synthetic credential and loopback TLS server.
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("fixture root exists");
    let nonce = env::var("DAKIA_MAIL_COMMAND_TLS_NONCE").expect("fixture nonce");
    assert_eq!(
        fs::read_to_string(root.join(".dakia-mail-command-tls")).expect("fixture ownership marker"),
        format!("{nonce}\n")
    );

    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read native fixture environment")
        .expect("mail command test requires the explicit fictional fixture");
    let store = tauri::async_runtime::block_on(Store::open(fixture.data_dir.join("dakia.db")))
        .expect("open owned fixture database");
    tauri::async_runtime::block_on(async {
        fixture.seed(&store).await.expect("seed fictional account");
        store
            .save_mailbox_catalog_state(
                fixture.account.id,
                "INBOX",
                "INBOX",
                EPOCH as u32,
                64,
                true,
            )
            .await
            .expect("save fictional inbox epoch");
        for mailbox in ["Archive", "Spam", "Trash"] {
            store
                .save_mailbox_catalog_state(
                    fixture.account.id,
                    mailbox,
                    mailbox,
                    EPOCH as u32,
                    0,
                    true,
                )
                .await
                .expect("save fictional mutation destination epoch");
        }
        for uid in [11, 12, 21, 22, 24, 25, 26, 99] {
            let message = summary(&fixture.account, "INBOX", uid);
            store
                .upsert_messages(std::slice::from_ref(&message))
                .await
                .expect("seed immutable command target");
        }
        let restore_target = summary(&fixture.account, "Spam", 23);
        store
            .upsert_messages(std::slice::from_ref(&restore_target))
            .await
            .expect("seed Spam restore target");
        let starred_id = format!("{}:INBOX:12", fixture.account.id);
        store
            .cache_message_content(
                &starred_id,
                false,
                CachedMessageContent {
                    body_text: "Cached fictional star body".into(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect(
                "seed cached star content so star hydration cannot open another provider session",
            );
    });

    let app = mock_builder()
        .manage(app_state(store.clone(), fixture.data_dir.clone()))
        .invoke_handler(tauri::generate_handler![
            set_message_read,
            set_message_starred,
            apply_mailbox_action,
        ])
        .build(super::test_support::mock_context())
        .expect("build production command dispatcher");
    let reader = WebviewWindowBuilder::new(&app, "reader-mail-command-tls", WebviewUrl::default())
        .build()
        .expect("build Reader-scoped test window");

    let current = |mailbox: &str, uid| {
        tauri::async_runtime::block_on(
            store.message(&format!("{}:{mailbox}:{uid}", fixture.account.id)),
        )
        .expect("read command target")
        .expect("command target exists")
    };

    let read_target = current("INBOX", 11);
    get_ipc_response(
        &reader,
        request(
            "set_message_read",
            json!({
                "messageId": read_target.id,
                "read": true,
                "expectedUidValidity": EPOCH,
            }),
        ),
    )
    .expect("exact read target completes through TLS IMAP");
    assert!(
        current("INBOX", 11).is_read,
        "read state publishes only after provider success"
    );

    let starred_target = current("INBOX", 12);
    let starred = response_json(
        get_ipc_response(
            &reader,
            request(
                "set_message_starred",
                json!({
                    "messageId": starred_target.id,
                    "starred": true,
                    "expectedUidValidity": EPOCH,
                }),
            ),
        )
        .expect("exact star target completes through TLS IMAP"),
    );
    assert_eq!(starred["uid"], 12);
    assert!(
        current("INBOX", 12).is_flagged,
        "star publishes only after provider success"
    );

    let actions = [
        ("INBOX", 21, "archive", "Archive"),
        ("INBOX", 22, "spam", "Spam"),
        ("Spam", 23, "not_spam", "INBOX"),
        ("INBOX", 24, "trash", "Trash"),
        ("INBOX", 25, "delete", ""),
    ];
    for (mailbox, uid, action, destination) in actions {
        let target = current(mailbox, uid);
        get_ipc_response(
            &reader,
            request(
                "apply_mailbox_action",
                json!({
                    "accountId": fixture.account.id,
                    "mailbox": mailbox,
                    "uid": uid,
                    "action": action,
                    "messageId": target.id,
                    "expectedUidValidity": EPOCH,
                }),
            ),
        )
        .unwrap_or_else(|failure| panic!("{action} exact target must complete: {failure}"));
        assert!(
            tauri::async_runtime::block_on(store.message(&target.id))
                .expect("read source after mutation")
                .is_none(),
            "{action} must remove only its committed source target"
        );
        if !destination.is_empty() {
            let moved = tauri::async_runtime::block_on(store.message_by_locator(
                fixture.account.id,
                destination,
                uid + 100,
            ))
            .expect("read mapped destination");
            assert!(
                moved.is_some(),
                "{action} must use the provider COPYUID destination"
            );
        }
    }

    // A stale click is rejected by the command's durable identity fence.  No
    // loopback session can be opened because validation precedes credentials.
    let transcript = PathBuf::from(
        env::var_os("DAKIA_MAIL_COMMAND_TLS_TRANSCRIPT")
            .expect("runner supplies a transcript path"),
    );
    let sessions_before_stale = recorded_session_count(&transcript);
    let stale_target = current("INBOX", 26);
    let stale = get_ipc_response(
        &reader,
        request(
            "set_message_read",
            json!({
                "messageId": stale_target.id,
                "read": true,
                "expectedUidValidity": STALE_EPOCH,
            }),
        ),
    )
    .expect_err("stale epoch must not reach the provider");
    assert!(stale.to_string().contains("This message changed"));
    assert!(
        !current("INBOX", 26).is_read,
        "stale rejection leaves local state intact"
    );
    let sessions_after_stale = recorded_session_count(&transcript);
    assert_eq!(
        sessions_after_stale, sessions_before_stale,
        "stale command must not open even an empty provider session"
    );

    // The fixture returns `NO` for this exact remote MOVE.  The command must
    // report failure and keep the original durable target for a later refresh.
    let failed_target = current("INBOX", 99);
    let failure = get_ipc_response(
        &reader,
        request(
            "apply_mailbox_action",
            json!({
                "accountId": fixture.account.id,
                "mailbox": "INBOX",
                "uid": 99,
                "action": "trash",
                "messageId": failed_target.id,
                "expectedUidValidity": EPOCH,
            }),
        ),
    )
    .expect_err("provider NO must fail the command");
    assert!(
        failure
            .to_string()
            .contains("requires IMAP UID MOVE support"),
        "a provider rejection remains a visible, non-local success"
    );
    assert_eq!(current("INBOX", 99).mailbox, "INBOX");

    let artifact = env::var_os("DAKIA_MAIL_COMMAND_TLS_ARTIFACT")
        .expect("runner supplies a command artifact path");
    fs::write(
        artifact,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "nonce": nonce,
            "accountId": fixture.account.id,
            "epoch": EPOCH,
            "completed": ["read", "star", "archive", "spam", "not_spam", "trash", "delete"],
            "staleRejected": true,
            "staleSessionsBefore": sessions_before_stale,
            "staleSessionsAfter": sessions_after_stale,
            "providerFailureRetainedSource": true,
        }))
        .expect("serialize command evidence"),
    )
    .expect("write command evidence");
}
