//! Reader hydration acceptance for the first, uncached Tauri command call.
//!
//! The runner supplies the same owned loopback TLS IMAP fixture and registered
//! MIME message used by the attachment command artifact lane.  This test keeps
//! the catalogue row header-only, so the first command must use the parsed
//! provider snapshot rather than the intentionally body-less store projection.

use super::*;
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

const UID_VALIDITY: u32 = 77;
const UID: u32 = 42;

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("Reader hydration must not invoke email classification")
    }
}

fn app_state(store: Store, data_dir: PathBuf) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir,
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "uncached-message-content-tls".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn header_only_message(account: &Account) -> MailSummary {
    MailSummary {
        id: format!("{}:INBOX:{UID}", account.id),
        account_id: account.id.to_string(),
        mailbox: "INBOX".into(),
        uid: i64::from(UID),
        mailbox_uid_validity: Some(i64::from(UID_VALIDITY)),
        message_id: Some("<fictional-attachment-command-artifact@localhost>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("{}:INBOX:{UID}", account.id),
        subject: "Fictional attachment command artifact".into(),
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

fn request(message_id: &str) -> InvokeRequest {
    command_request(
        "message_content",
        json!({
            "messageId": message_id,
            "expectedUidValidity": UID_VALIDITY,
        }),
    )
}

fn command_request(command: &str, body: Value) -> InvokeRequest {
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

fn response_json(body: InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(value) => serde_json::from_str(&value).expect("IPC JSON"),
        InvokeResponseBody::Raw(_) => panic!("message_content returned raw bytes"),
    }
}

fn owned_root_and_nonce() -> (PathBuf, String) {
    let root = env::var_os("DAKIA_MESSAGE_CONTENT_UNCACHED_TLS_ROOT")
        .expect("uncached Reader test requires the owned TLS fixture runner");
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("owned fixture root exists");
    let nonce = env::var("DAKIA_MESSAGE_CONTENT_UNCACHED_TLS_NONCE")
        .expect("uncached Reader fixture nonce");
    assert_eq!(
        fs::read_to_string(root.join(".dakia-message-content-uncached-tls"))
            .expect("read fixture ownership nonce"),
        format!("{nonce}\n")
    );
    (root, nonce)
}

fn write_execution_receipt(root: &PathBuf, nonce: &str, test: &str) {
    let receipt = PathBuf::from(
        env::var_os("DAKIA_MESSAGE_CONTENT_UNCACHED_TLS_RECEIPT")
            .expect("owned TLS runner must require an execution receipt"),
    );
    assert!(receipt.is_absolute(), "execution receipt must be absolute");
    assert_eq!(
        receipt
            .parent()
            .expect("execution receipt parent")
            .canonicalize()
            .expect("execution receipt parent exists"),
        *root,
        "execution receipt must stay in the owned fixture root"
    );
    let mut receipt_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&receipt)
        .expect("create one fresh execution receipt after assertions");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        receipt_file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .expect("restrict execution receipt permissions");
    }
    serde_json::to_writer(
        &mut receipt_file,
        &json!({
            "schemaVersion": 1,
            "nonce": nonce,
            "test": test,
        }),
    )
    .expect("serialize execution receipt");
    receipt_file
        .write_all(b"\n")
        .expect("finish execution receipt");
}

#[test]
#[ignore = "requires owned TLS fixture runner"]
fn first_uncached_reader_command_keeps_provider_mime_body_and_downloadable_attachments() {
    let (root, nonce) = owned_root_and_nonce();

    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read fictional TLS fixture environment")
        .expect("uncached Reader test requires the explicit fixture");
    let store = tauri::async_runtime::block_on(Store::open(fixture.data_dir.join("dakia.db")))
        .expect("open owned fixture store");
    let target = tauri::async_runtime::block_on(async {
        fixture.seed(&store).await.expect("seed fictional account");
        store
            .save_mailbox_catalog_state(fixture.account.id, "INBOX", "INBOX", UID_VALIDITY, 1, true)
            .await
            .expect("seed current mailbox epoch");
        let target = header_only_message(&fixture.account);
        store
            .upsert_messages(std::slice::from_ref(&target))
            .await
            .expect("seed header-only catalogue row");
        target
    });

    let state = app_state(store.clone(), fixture.data_dir.clone());
    let diagnostic_state = state.clone();
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![message_content])
        .build(super::test_support::mock_context())
        .expect("build production Tauri command dispatcher");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-uncached-message-content-tls",
        WebviewUrl::default(),
    )
    .build()
    .expect("build Reader window");

    let first = match get_ipc_response(&reader, request(&target.id)) {
        Ok(response) => response_json(response),
        Err(envelope) => {
            let detail = match tauri::async_runtime::block_on(load_message_content(
                &diagnostic_state,
                &target.id,
                Some(i64::from(UID_VALIDITY)),
            )) {
                Ok(content) => format!(
                    "loader unexpectedly succeeded with {} body bytes and {} attachments",
                    content.body_text.len(),
                    content.attachments.len()
                ),
                Err(error) => error,
            };
            panic!(
                "first uncached Reader command fetches the provider MIME: {envelope}; loader detail: {detail}"
            );
        }
    };
    assert!(
        first["body_text"]
            .as_str()
            .expect("Reader body text")
            .contains("Fictional provider MIME reaches the Reader"),
        "the first Reader response must retain the parsed provider body"
    );
    let filenames = first["attachments"]
        .as_array()
        .expect("Reader attachment list")
        .iter()
        .map(|attachment| {
            attachment["filename"]
                .as_str()
                .expect("attachment filename")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        filenames,
        ["quarterly-report.pdf.command", "meeting-notes.txt"],
        "the first Reader response must retain downloadable MIME metadata"
    );

    let canonical = tauri::async_runtime::block_on(store.message(&target.id))
        .expect("read committed catalogue row")
        .expect("message remains committed");
    assert!(canonical.body_text.is_empty());
    assert!(canonical.attachments.is_empty());
    assert!(
        tauri::async_runtime::block_on(store.cached_message_content(&target.id))
            .expect("read persisted Reader cache")
            .is_some(),
        "the complete response must be cached after the first command"
    );

    // A bad endpoint makes a second provider fetch fail. The repeated command
    // must still succeed from the durable cache, proving the cache-hit branch
    // rather than simply repeating the same fresh fetch.
    let mut offline = fixture.account.clone();
    offline.imap_port = 1;
    tauri::async_runtime::block_on(store.save_account(&offline))
        .expect("make a second provider connection impossible");
    let repeated = response_json(
        get_ipc_response(&reader, request(&target.id))
            .expect("cached Reader command must not contact the provider"),
    );
    assert_eq!(
        repeated, first,
        "cached Reader response preserves MIME semantics"
    );
    write_execution_receipt(
        &root,
        &nonce,
        "first_uncached_reader_command_keeps_provider_mime_body_and_downloadable_attachments",
    );
}

#[test]
#[ignore = "requires owned TLS fixture runner"]
fn reader_command_rejects_a_pending_same_epoch_provider_namespace_replacement() {
    let (root, nonce) = owned_root_and_nonce();
    let replacement_port = env::var("DAKIA_MESSAGE_CONTENT_REPLACEMENT_IMAP_PORT")
        .expect("owned TLS runner must provide the replacement provider port")
        .parse::<u16>()
        .expect("replacement provider port must be valid");
    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read fictional TLS fixture environment")
        .expect("namespace replacement test requires the explicit fixture");
    assert_ne!(replacement_port, fixture.account.imap_port);

    let race_data_dir = root.join("namespace-race-profile");
    assert_eq!(
        fs::read_to_string(race_data_dir.join(".dakia-native-mail-safety-profile"))
            .expect("read race profile ownership marker"),
        NATIVE_MAIL_SAFETY_FIXTURE_MARKER
    );
    let store = tauri::async_runtime::block_on(Store::open(race_data_dir.join("dakia.db")))
        .expect("open owned namespace-race store");
    let target = tauri::async_runtime::block_on(async {
        fixture
            .seed(&store)
            .await
            .expect("seed first fictional provider");
        store
            .save_mailbox_catalog_state(fixture.account.id, "INBOX", "INBOX", UID_VALIDITY, 1, true)
            .await
            .expect("seed current mailbox epoch");
        let target = header_only_message(&fixture.account);
        store
            .upsert_messages(std::slice::from_ref(&target))
            .await
            .expect("seed first-provider catalogue row");
        target
    });
    let state = app_state(store.clone(), race_data_dir);
    // Exercise the same production helper update_account uses. Both fictional
    // providers deliberately use the same mailbox, UID, and UIDVALIDITY; only
    // the provider body differs.
    tauri::async_runtime::block_on(async {
        let mut replacement = fixture.account.clone();
        replacement.imap_port = replacement_port;
        save_account_with_rebuild_intent(
            &state,
            &replacement,
            true,
            None,
            "fixture-namespace-replacement",
        )
        .await
        .expect("atomically persist replacement namespace and reset intent");
    });
    let diagnostic_state = state.clone();
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![message_content])
        .build(super::test_support::mock_context())
        .expect("build production Tauri command dispatcher");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-namespace-replacement-tls",
        WebviewUrl::default(),
    )
    .build()
    .expect("build Reader window");

    if let Ok(response) = get_ipc_response(&reader, request(&target.id)) {
        let returned = response_json(response);
        panic!(
            "Reader must not return same-epoch content from the replacement provider: {returned}"
        );
    }
    let loader_error = tauri::async_runtime::block_on(load_message_content(
        &diagnostic_state,
        &target.id,
        Some(i64::from(UID_VALIDITY)),
    ));
    assert!(
        matches!(loader_error, Err(ref error) if error == &account_namespace_reset_pending_error()),
        "the durable replacement intent must fence the cache and provider fetch"
    );
    let canonical = tauri::async_runtime::block_on(store.message(&target.id))
        .expect("read retained first-provider catalogue row")
        .expect("old row stays visible until reset sync replaces it");
    assert!(canonical.body_text.is_empty());
    assert!(canonical.attachments.is_empty());
    assert!(
        tauri::async_runtime::block_on(store.cached_message_content(&target.id))
            .expect("read Reader cache after replacement rejection")
            .is_none(),
        "replacement-provider MIME must neither return nor enter the old row cache"
    );
    write_execution_receipt(
        &root,
        &nonce,
        "reader_command_rejects_a_pending_same_epoch_provider_namespace_replacement",
    );
}

#[test]
#[ignore = "requires owned TLS fixture runner"]
fn attachment_commands_reject_a_pending_same_epoch_provider_namespace_replacement() {
    let (root, nonce) = owned_root_and_nonce();
    let replacement_port = env::var("DAKIA_MESSAGE_CONTENT_REPLACEMENT_IMAP_PORT")
        .expect("owned TLS runner must provide the replacement provider port")
        .parse::<u16>()
        .expect("replacement provider port must be valid");
    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read fictional TLS fixture environment")
        .expect("namespace replacement test requires the explicit fixture");
    assert_ne!(replacement_port, fixture.account.imap_port);

    let race_data_dir = root.join("namespace-attachment-race-profile");
    assert_eq!(
        fs::read_to_string(race_data_dir.join(".dakia-native-mail-safety-profile"))
            .expect("read attachment-race profile ownership marker"),
        NATIVE_MAIL_SAFETY_FIXTURE_MARKER
    );
    let store = tauri::async_runtime::block_on(Store::open(race_data_dir.join("dakia.db")))
        .expect("open owned attachment-race store");
    let target = tauri::async_runtime::block_on(async {
        fixture
            .seed(&store)
            .await
            .expect("seed first fictional provider");
        store
            .save_mailbox_catalog_state(fixture.account.id, "INBOX", "INBOX", UID_VALIDITY, 1, true)
            .await
            .expect("seed current mailbox epoch");
        let target = MailService::new(store.clone())
            .fetch_full_message(&fixture.account, "INBOX", UID)
            .await
            .expect("fetch first-provider attachment metadata");
        store
            .upsert_messages(std::slice::from_ref(&target))
            .await
            .expect("retain first-provider catalogue row");
        target
    });
    let attachment_id = target.attachments[0].attachment.id.clone();
    let state = app_state(store.clone(), race_data_dir);
    tauri::async_runtime::block_on(async {
        let mut replacement = fixture.account.clone();
        replacement.imap_port = replacement_port;
        save_account_with_rebuild_intent(
            &state,
            &replacement,
            true,
            None,
            "fixture-attachment-namespace-replacement",
        )
        .await
        .expect("atomically persist attachment replacement namespace and reset intent");
    });
    // The fixture transport separately validates its environment allowlist.
    // This exact ignored test runs alone, so move that loopback-only allowlist
    // to the replacement provider after the first provider supplied the row.
    unsafe {
        env::set_var(
            "DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT",
            replacement_port.to_string(),
        );
    }
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            save_attachment,
            export_message,
            save_all_attachments,
            forward_attachments,
        ])
        .build(super::test_support::mock_context())
        .expect("build production attachment command dispatcher");
    let reader = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("build attachment Reader window");

    for (command, body) in [
        (
            "save_attachment",
            json!({
                "messageId": target.id,
                "attachmentId": attachment_id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
        (
            "export_message",
            json!({
                "messageId": target.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
        (
            "save_all_attachments",
            json!({
                "messageId": target.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
        (
            "forward_attachments",
            json!({
                "messageId": target.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
    ] {
        let failure = get_ipc_response(&reader, command_request(command, body))
            .expect_err("attachment command must reject the replacement provider");
        assert_eq!(
            failure,
            account_namespace_reset_pending_error(),
            "{command} must fail at the durable namespace fence before it can expose replacement data"
        );
    }
    let downloads = PathBuf::from(
        env::var("DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR").expect("fixture Downloads directory"),
    );
    assert!(
        fs::read_dir(downloads)
            .expect("read Downloads after rejected attachment commands")
            .next()
            .is_none(),
        "the replacement provider must not create a file"
    );
    write_execution_receipt(
        &root,
        &nonce,
        "attachment_commands_reject_a_pending_same_epoch_provider_namespace_replacement",
    );
}

#[test]
#[ignore = "requires owned TLS fixture runner"]
fn mutation_commands_reject_a_pending_same_epoch_provider_namespace_replacement() {
    let (root, nonce) = owned_root_and_nonce();
    let replacement_port = env::var("DAKIA_MESSAGE_CONTENT_REPLACEMENT_IMAP_PORT")
        .expect("owned TLS runner must provide the replacement provider port")
        .parse::<u16>()
        .expect("replacement provider port must be valid");
    let replacement_transcript = PathBuf::from(
        env::var_os("DAKIA_MESSAGE_CONTENT_REPLACEMENT_IMAP_TRANSCRIPT")
            .expect("owned TLS runner must provide replacement provider transcript"),
    );
    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read fictional TLS fixture environment")
        .expect("namespace replacement test requires the explicit fixture");
    assert_ne!(replacement_port, fixture.account.imap_port);

    let race_data_dir = root.join("namespace-mutation-race-profile");
    assert_eq!(
        fs::read_to_string(race_data_dir.join(".dakia-native-mail-safety-profile"))
            .expect("read mutation-race profile ownership marker"),
        NATIVE_MAIL_SAFETY_FIXTURE_MARKER
    );
    let store = tauri::async_runtime::block_on(Store::open(race_data_dir.join("dakia.db")))
        .expect("open owned mutation-race store");
    let target = tauri::async_runtime::block_on(async {
        fixture
            .seed(&store)
            .await
            .expect("seed first fictional provider");
        store
            .save_mailbox_catalog_state(fixture.account.id, "INBOX", "INBOX", UID_VALIDITY, 1, true)
            .await
            .expect("seed current mailbox epoch");
        let target = header_only_message(&fixture.account);
        store
            .upsert_messages(std::slice::from_ref(&target))
            .await
            .expect("retain first-provider mutation target");
        target
    });
    let state = app_state(store.clone(), race_data_dir);
    tauri::async_runtime::block_on(async {
        let mut replacement = fixture.account.clone();
        replacement.imap_port = replacement_port;
        save_account_with_rebuild_intent(
            &state,
            &replacement,
            true,
            None,
            "fixture-mutation-namespace-replacement",
        )
        .await
        .expect("atomically persist mutation replacement namespace and reset intent");
    });
    // If the gate regresses, the fixture transport must accept only the
    // replacement endpoint so its transcript can prove that a mutation escaped
    // the durable reset fence. This exact ignored test runs in its own process.
    unsafe {
        env::set_var(
            "DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT",
            replacement_port.to_string(),
        );
    }
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            set_message_read,
            set_message_starred,
            apply_mailbox_action,
        ])
        .build(super::test_support::mock_context())
        .expect("build production mutation command dispatcher");
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("build mutation command window");

    for (command, body) in [
        (
            "set_message_read",
            json!({
                "messageId": target.id,
                "read": true,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
        (
            "set_message_starred",
            json!({
                "messageId": target.id,
                "starred": true,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
        (
            "apply_mailbox_action",
            json!({
                "accountId": fixture.account.id,
                "mailbox": "INBOX",
                "uid": UID,
                "action": "archive",
                "messageId": target.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
    ] {
        let failure = get_ipc_response(&main, command_request(command, body))
            .expect_err("mutation command must reject the replacement provider");
        assert_eq!(
            failure,
            account_namespace_reset_pending_error(),
            "{command} must fail at the durable namespace fence before provider mutation"
        );
    }
    let retained = tauri::async_runtime::block_on(store.message(&target.id))
        .expect("read retained target")
        .expect("pending reset must retain old target without local mutation");
    assert!(
        !retained.is_read,
        "rejected read command cannot change local state"
    );
    assert!(
        !retained.is_flagged,
        "rejected star command cannot change local state"
    );
    assert_eq!(retained.mailbox, "INBOX");
    assert_eq!(
        fs::read_to_string(&replacement_transcript).expect("read replacement fixture transcript"),
        "",
        "pending namespace replacement must not open a provider session or write a remote mutation"
    );
    write_execution_receipt(
        &root,
        &nonce,
        "mutation_commands_reject_a_pending_same_epoch_provider_namespace_replacement",
    );
}
