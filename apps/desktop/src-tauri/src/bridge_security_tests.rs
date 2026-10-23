//! Tauri dispatcher regressions for the application command capability map.
//!
//! These run commands through Tauri's MockRuntime dispatcher.  They exercise
//! the generated application ACL instead of treating the JSON capability files
//! as sufficient evidence.  A shadow-root email renderer shares a JavaScript
//! realm with its Reader window, so this does not replace HTML sanitisation or
//! a sandboxed-renderer acceptance test.

use super::*;
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

fn app() -> tauri::App<tauri::test::MockRuntime> {
    mock_builder()
        .manage(NativeNotificationAvailability::default())
        .manage(Arc::new(SendRecoveryRuntime::default()))
        .invoke_handler(tauri::generate_handler![
            provider_presets,
            translation_models,
            send_message,
            list_send_recoveries,
            open_send_recovery_window,
            read_send_recovery_seed,
            acknowledge_send_recovery_seed,
            mail_rebuild_status,
            open_external_url,
            open_compose_window,
            open_reader_window,
            open_settings_window,
            open_account_window,
            send_desktop_notification,
        ])
        .build(super::test_support::mock_context())
        .expect("the production Tauri capability manifest must build")
}

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("bridge command tests must not classify mail")
    }
}

fn test_state(store: Store) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir: PathBuf::new(),
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "bridge-security-test".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn app_with_state(state: Arc<AppState>) -> tauri::App<tauri::test::MockRuntime> {
    app_with_state_and_runtime(state, Arc::new(SendRecoveryRuntime::default()))
}

fn app_with_state_and_runtime(
    state: Arc<AppState>,
    runtime: Arc<SendRecoveryRuntime>,
) -> tauri::App<tauri::test::MockRuntime> {
    mock_builder()
        .manage(state)
        .manage(NativeNotificationAvailability::default())
        .manage(runtime)
        .invoke_handler(tauri::generate_handler![
            send_message,
            list_send_recoveries,
            open_send_recovery_window,
            read_send_recovery_seed,
            acknowledge_send_recovery_seed,
        ])
        .build(super::test_support::mock_context())
        .expect("the production send recovery command dispatcher must build")
}

fn fatal_recovery_app(
    exit_status: StartupFailureExitStatus,
) -> tauri::App<tauri::test::MockRuntime> {
    mock_builder()
        .manage(exit_status)
        .invoke_handler(tauri::generate_handler![quit_startup_recovery])
        .build(super::test_support::mock_context())
        .expect("the fatal recovery capability manifest must build")
}

fn notification_body() -> serde_json::Value {
    serde_json::json!({
        "notification": {
            "title": "Fictional notification",
            "body": "No native notification may be shown without a verified identity",
            "accountId": "62a9320c-04f5-47c2-b4fa-d4952f0b6121",
            "messageId": "62a9320c-04f5-47c2-b4fa-d4952f0b6121:INBOX:1",
            "threadId": "fictional-notification-thread",
            "rfcMessageId": "<fictional-notification@localhost>",
            "count": 1,
            "sound": null
        }
    })
}

fn request(command: &str, url: &str) -> InvokeRequest {
    request_with_body(command, url, serde_json::Value::Null)
}

fn request_with_body(command: &str, url: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: command.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: url.parse().expect("test URL must be valid"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn rejection(response: Result<tauri::ipc::InvokeResponseBody, serde_json::Value>) -> String {
    response
        .expect_err("the dispatcher must reject this command")
        .to_string()
}

fn response_string(body: tauri::ipc::InvokeResponseBody) -> String {
    body.deserialize::<String>()
        .expect("window factory response must be a JSON string")
}

fn acl_rejection(detail: &str) -> bool {
    detail.contains("not allowed")
}

#[test]
fn capability_dispatcher_accepts_only_the_local_window_that_owns_a_command() {
    let app = app();
    let account = WebviewWindowBuilder::new(&app, "account", WebviewUrl::default())
        .build()
        .expect("account window");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-0123456789abcdef0123456789abcdef",
        WebviewUrl::default(),
    )
    .build()
    .expect("reader window");
    let settings = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::default())
        .build()
        .expect("settings window");

    let presets = get_ipc_response(&account, request("provider_presets", "tauri://localhost"))
        .expect("the account setup window may load provider presets");
    assert!(presets
        .deserialize::<serde_json::Value>()
        .expect("provider presets must be JSON")
        .is_array());

    let reader_models = rejection(get_ipc_response(
        &reader,
        request("translation_models", "tauri://localhost"),
    ));
    assert!(
        !acl_rejection(&reader_models),
        "the Reader capability must admit translation-model queries: {reader_models}"
    );

    let settings_models = rejection(get_ipc_response(
        &settings,
        request("translation_models", "tauri://localhost"),
    ));
    assert!(
        !acl_rejection(&settings_models),
        "the Settings capability must admit translation-model queries: {settings_models}"
    );

    let reader_provider_setup = rejection(get_ipc_response(
        &reader,
        request("provider_presets", "tauri://localhost"),
    ));
    assert!(
        acl_rejection(&reader_provider_setup),
        "Reader must not acquire account-setup authority: {reader_provider_setup}"
    );

    let account_translation = rejection(get_ipc_response(
        &account,
        request("translation_models", "tauri://localhost"),
    ));
    assert!(
        acl_rejection(&account_translation),
        "Account setup must not acquire Reader translation authority: {account_translation}"
    );
}

#[test]
fn recovery_commands_are_scoped_to_main_listing_and_exact_compose_children() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let reader = WebviewWindowBuilder::new(&app, "reader-fictional", WebviewUrl::default())
        .build()
        .expect("reader window");
    let compose = WebviewWindowBuilder::new(
        &app,
        "compose-8df35ad0-a609-4600-ae5c-0123456789ab",
        WebviewUrl::default(),
    )
    .build()
    .expect("compose window");
    let key = "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6";

    let recovery_target = serde_json::json!({
        "accountId": "1e4d64b5-a09a-4598-bc7a-cdd763edb239",
        "attemptId": "2d24d98b-6419-4bc5-8ac5-f6a7ab4fcb73"
    });
    for (command, payload) in [
        (
            "list_send_recoveries",
            serde_json::json!({ "accountIds": [], "cursor": null }),
        ),
        ("open_send_recovery_window", recovery_target),
    ] {
        let main_result = get_ipc_response(
            &main,
            request_with_body(command, "tauri://localhost", payload.clone()),
        );
        if let Err(detail) = main_result {
            assert!(
                !acl_rejection(&detail.to_string()),
                "main must pass the generated dispatcher ACL for {command}: {detail}"
            );
        }
        let denied = rejection(get_ipc_response(
            &reader,
            request_with_body(command, "tauri://localhost", payload),
        ));
        assert!(
            acl_rejection(&denied),
            "Reader must not list or open recovery: {denied}"
        );
    }

    for command in ["read_send_recovery_seed", "acknowledge_send_recovery_seed"] {
        let compose_result = get_ipc_response(
            &compose,
            request_with_body(
                command,
                "tauri://localhost",
                serde_json::json!({ "recoveryKey": key }),
            ),
        );
        if let Err(detail) = compose_result {
            assert!(
                !acl_rejection(&detail.to_string()),
                "Compose must reach its recovery handler before grant validation: {detail}"
            );
        }
        for (name, window) in [("main", &main), ("reader", &reader)] {
            let denied = rejection(get_ipc_response(
                window,
                request_with_body(
                    command,
                    "tauri://localhost",
                    serde_json::json!({ "recoveryKey": key }),
                ),
            ));
            assert!(
                acl_rejection(&denied),
                "{name} must not read or acknowledge a Compose recovery: {denied}"
            );
        }
    }
}

#[test]
fn fatal_recovery_exit_requires_the_exact_capability_window_and_active_startup_failure() {
    let inactive_status = StartupFailureExitStatus::default();
    let inactive_app = fatal_recovery_app(inactive_status.clone());
    let main = WebviewWindowBuilder::new(&inactive_app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let fatal = WebviewWindowBuilder::new(&inactive_app, "fatal-recovery", WebviewUrl::default())
        .build()
        .expect("fatal recovery window");

    let main_error = rejection(get_ipc_response(
        &main,
        request("quit_startup_recovery", "tauri://localhost"),
    ));
    assert!(
        acl_rejection(&main_error),
        "main must never acquire fatal recovery exit authority: {main_error}"
    );

    let inactive_error = rejection(get_ipc_response(
        &fatal,
        request("quit_startup_recovery", "tauri://localhost"),
    ));
    assert!(
        !acl_rejection(&inactive_error) && inactive_error.contains("unavailable"),
        "the exact fatal window must reach the state guard, not exit during normal startup: {inactive_error}"
    );
    assert_eq!(inactive_status.resolve(0), 0);

    // MockRuntime deliberately panics for AppHandle::exit. The state guard is
    // exercised here without calling that runtime-only primitive; the focused
    // StartupFailureExitStatus test covers its nonzero outcome.
    let active_status = StartupFailureExitStatus::default();
    active_status.activate_fatal_recovery();
    assert!(active_status.may_exit_from_fatal_recovery_window("fatal-recovery"));
    active_status.mark_requested();
    assert_eq!(active_status.resolve(0), 1);
}

#[tokio::test]
async fn disabled_account_send_through_the_compose_dispatcher_keeps_a_retryable_recovery() {
    let store = Store::in_memory().await.expect("in-memory store");
    let mut account = AccountDraft {
        email: "disabled-recovery@example.test".into(),
        display_name: "Disabled recovery".into(),
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
    account.enabled = false;
    store
        .save_account(&account)
        .await
        .expect("save disabled account");
    let attempt_id = Uuid::new_v4();
    let app = app_with_state(test_state(store.clone()));
    let compose = WebviewWindowBuilder::new(
        &app,
        format!("compose-{}", Uuid::new_v4()),
        WebviewUrl::default(),
    )
    .build()
    .expect("compose window");
    let response = rejection(get_ipc_response(
        &compose,
        request_with_body(
            "send_message",
            "tauri://localhost",
            serde_json::json!({
                "draft": {
                    "account_id": account.id,
                    "to": ["recipient@example.test"],
                    "cc": [],
                    "bcc": [],
                    "subject": "Disabled account recovery",
                    "body_text": "Fictional draft remains recoverable.",
                    "body_html": null,
                    "in_reply_to": null,
                    "references": null,
                    "attachments": [],
                    "send_attempt_id": attempt_id,
                }
            }),
        ),
    ));
    assert!(
        !acl_rejection(&response) && response.contains("not_submitted"),
        "the production Compose command must fail after durable preparation: {response}"
    );
    let recovery = store
        .load_send_recovery(account.id, attempt_id)
        .await
        .expect("load recovery")
        .expect("disabled account failure keeps its exact draft");
    assert_eq!(
        recovery.status,
        dakia_core::storage::SendRecoveryStatus::NotSubmitted
    );
    assert_eq!(recovery.message.subject, "Disabled account recovery");
}

#[tokio::test]
async fn main_recovery_list_hides_a_retryable_row_during_its_active_resend() {
    let store = Store::in_memory().await.expect("in-memory store");
    let account = AccountDraft {
        email: "active-retry@example.test".into(),
        display_name: "Active retry".into(),
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
    store.save_account(&account).await.expect("save account");
    let attempt_id = Uuid::new_v4();
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Active retry must stay hidden".into(),
        body_text: "Fictional retry content".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    store
        .prepare_send_recovery(account.id, attempt_id, &draft)
        .await
        .expect("persist recovery");
    store
        .release_send_recovery(account.id, attempt_id)
        .await
        .expect("make recovery retryable");
    let runtime = Arc::new(SendRecoveryRuntime::default());
    let app = app_with_state_and_runtime(test_state(store), Arc::clone(&runtime));
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let active = runtime
        .reserve(account.id, attempt_id, app.handle().clone(), true)
        .expect("reserve active retry before the new prepare commits");

    let response = get_ipc_response(
        &main,
        request_with_body(
            "list_send_recoveries",
            "tauri://localhost",
            serde_json::json!({ "accountIds": [account.id], "cursor": null }),
        ),
    )
    .expect("main recovery list command");
    let page = response
        .deserialize::<serde_json::Value>()
        .expect("recovery list JSON");
    assert_eq!(page["items"], serde_json::json!([]));
    assert_eq!(page["total_count"], 0);
    drop(active);
}

#[test]
fn settings_reaches_only_the_identity_gated_notification_command() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let settings = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::default())
        .build()
        .expect("settings window");
    let reader = WebviewWindowBuilder::new(&app, "reader-fictional", WebviewUrl::default())
        .build()
        .expect("reader window");

    for (name, window) in [("main", &main), ("settings", &settings)] {
        let error = rejection(get_ipc_response(
            window,
            request_with_body(
                "send_desktop_notification",
                "tauri://localhost",
                notification_body(),
            ),
        ));
        assert!(
            !acl_rejection(&error),
            "{name} must reach the identity-gated notification handler: {error}"
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            error.trim_matches('"'),
            NATIVE_NOTIFICATION_UNAVAILABLE_ERROR
        );
        #[cfg(not(target_os = "macos"))]
        assert!(error.contains("only used on macOS"));
    }

    let reader_error = rejection(get_ipc_response(
        &reader,
        request_with_body(
            "send_desktop_notification",
            "tauri://localhost",
            notification_body(),
        ),
    ));
    assert!(
        acl_rejection(&reader_error),
        "Reader must not acquire notification delivery authority: {reader_error}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_main_and_settings_cannot_bypass_identity_gating_with_plugin_notify() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let settings = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::default())
        .build()
        .expect("settings window");

    for (name, window) in [("main", &main), ("settings", &settings)] {
        for command in ["plugin:notification|notify", "plugin:notification|show"] {
            let error = rejection(get_ipc_response(
                window,
                request_with_body(
                    command,
                    "tauri://localhost",
                    serde_json::json!({
                        "options": { "title": "Spoof", "body": "must be rejected" },
                        "id": 1
                    }),
                ),
            ));
            assert!(
                acl_rejection(&error),
                "{name} must not bypass notification identity gating through {command}: {error}"
            );
        }
    }
}

#[test]
fn mail_rebuild_status_is_available_only_to_main_and_settings() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let settings = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::default())
        .build()
        .expect("settings window");
    let reader = WebviewWindowBuilder::new(&app, "reader-fictional", WebviewUrl::default())
        .build()
        .expect("reader window");
    let compose = WebviewWindowBuilder::new(&app, "compose-fictional", WebviewUrl::default())
        .build()
        .expect("compose window");
    let account = WebviewWindowBuilder::new(&app, "account", WebviewUrl::default())
        .build()
        .expect("account window");

    for (name, window) in [("main", &main), ("settings", &settings)] {
        let result = get_ipc_response(window, request("mail_rebuild_status", "tauri://localhost"));
        let failure = result.expect_err("the test runtime deliberately has no AppState");
        let detail = failure.to_string();
        assert!(
            detail.contains("state not managed for field `state`"),
            "{name} must reach the real mail_rebuild_status handler after ACL: {detail}"
        );
    }

    for (name, window) in [
        ("reader", &reader),
        ("compose", &compose),
        ("account", &account),
    ] {
        let failure = rejection(get_ipc_response(
            window,
            request("mail_rebuild_status", "tauri://localhost"),
        ));
        assert!(
            acl_rejection(&failure),
            "{name} must not inspect rebuild state: {failure}"
        );
    }
}

#[test]
fn capability_dispatcher_rejects_remote_and_unlisted_windows_before_handlers_run() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let compose = WebviewWindowBuilder::new(&app, "compose-fictional", WebviewUrl::default())
        .build()
        .expect("compose window");
    let unlisted = WebviewWindowBuilder::new(&app, "untrusted-content", WebviewUrl::default())
        .build()
        .expect("unlisted test window");

    let remote = rejection(get_ipc_response(
        &main,
        request("translation_models", "https://evil.example.test/"),
    ));
    assert!(
        acl_rejection(&remote),
        "a remote origin must not invoke a custom command: {remote}"
    );

    let unknown_window = rejection(get_ipc_response(
        &unlisted,
        request("provider_presets", "tauri://localhost"),
    ));
    assert!(
        acl_rejection(&unknown_window),
        "an unlisted local window must not inherit application commands: {unknown_window}"
    );

    let compose_reader_models = rejection(get_ipc_response(
        &compose,
        request("translation_models", "tauri://localhost"),
    ));
    assert!(
        acl_rejection(&compose_reader_models),
        "Compose must not acquire Reader translation authority: {compose_reader_models}"
    );

    let compose_send = get_ipc_response(&compose, request("send_message", "tauri://localhost"));
    if let Err(error) = compose_send {
        let detail = error.to_string();
        assert!(
            !acl_rejection(&detail),
            "Compose owns send_message, so any failure must occur after ACL: {detail}"
        );
    }
}

#[test]
fn native_link_command_rejects_executable_and_local_schemes_through_ipc() {
    let app = app();
    let reader = WebviewWindowBuilder::new(&app, "reader-fictional", WebviewUrl::default())
        .build()
        .expect("reader window");

    for url in [
        "javascript:alert(1)",
        "file:///Users/fictional/.ssh/id_rsa",
        "data:text/html,fictional",
        "asset://localhost/private",
        "https://user:password@example.test/",
        "//example.test/",
    ] {
        let error = rejection(get_ipc_response(
            &reader,
            request_with_body(
                "open_external_url",
                "tauri://localhost",
                serde_json::json!({ "url": url }),
            ),
        ));
        assert!(
            error.contains("Only web and email links can be opened")
                || error.contains("cannot contain login details")
                || error.contains("This link is not valid"),
            "native IPC must reject {url:?} before invoking the opener: {error}"
        );
    }
}

#[test]
fn reader_cannot_bypass_the_validated_link_command_with_the_opener_plugin() {
    let app = app();
    let reader = WebviewWindowBuilder::new(&app, "reader-fictional", WebviewUrl::default())
        .build()
        .expect("reader window");

    for command in ["plugin:opener|open_url", "plugin:opener|reveal_item_in_dir"] {
        let error = rejection(get_ipc_response(
            &reader,
            request_with_body(
                command,
                "tauri://localhost",
                serde_json::json!({ "url": "tel:+15550123", "path": "/Users/fictional/private" }),
            ),
        ));
        assert!(
            acl_rejection(&error),
            "Reader must use Dakia's validated link command, not {command}: {error}"
        );
    }

    let notification = rejection(get_ipc_response(
        &reader,
        request_with_body(
            "plugin:notification|notify",
            "tauri://localhost",
            serde_json::json!({ "options": { "title": "Spoof", "body": "message" } }),
        ),
    ));
    assert!(
        acl_rejection(&notification),
        "Reader must not bypass Dakia's redacted notification path: {notification}"
    );
}

#[test]
fn reader_cannot_mint_a_window_with_another_capability_label() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-0123456789abcdef0123456789abcdef",
        WebviewUrl::default(),
    )
    .build()
    .expect("reader window");

    let generic_create = rejection(get_ipc_response(
        &reader,
        request_with_body(
            "plugin:webview|create_webview_window",
            "tauri://localhost",
            serde_json::json!({ "label": "settings", "url": "/?view=settings" }),
        ),
    ));
    assert!(
        acl_rejection(&generic_create),
        "Reader must not mint a settings-capability window: {generic_create}"
    );

    let settings = rejection(get_ipc_response(
        &reader,
        request_with_body(
            "open_settings_window",
            "tauri://localhost",
            serde_json::json!({ "accountId": null }),
        ),
    ));
    assert!(
        acl_rejection(&settings),
        "Reader must not ask Rust to mint a settings-capability window: {settings}"
    );

    let forged_reader = rejection(get_ipc_response(
        &main,
        request_with_body(
            "open_reader_window",
            "tauri://localhost",
            serde_json::json!({
                "label": "settings",
                "seedKey": "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6"
            }),
        ),
    ));
    assert!(
        forged_reader.contains("Reader window label is invalid"),
        "the Rust window factory must not turn a reader request into another capability: {forged_reader}"
    );

    let compose = get_ipc_response(
        &reader,
        request_with_body(
            "open_compose_window",
            "tauri://localhost",
            serde_json::json!({ "seedKey": "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6", "seedDbKey": null }),
        ),
    );
    let compose_label =
        response_string(compose.expect("Reader may mint only a Rust-owned compose window"));
    assert!(
        compose_label.starts_with("compose-"),
        "the Rust factory must return its generated child label"
    );

    let account = WebviewWindowBuilder::new(&app, "account", WebviewUrl::default())
        .build()
        .expect("account window");
    let denied = rejection(get_ipc_response(
        &account,
        request_with_body(
            "open_compose_window",
            "tauri://localhost",
            serde_json::json!({ "seedKey": "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6", "seedDbKey": null }),
        ),
    ));
    assert!(
        acl_rejection(&denied),
        "only main and Reader may create Compose: {denied}"
    );
}

#[test]
fn reader_window_accepts_one_opaque_seed_and_rejects_ambiguous_handoffs() {
    let seed = "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6";
    assert_eq!(
        reader_window_path(None, Some(seed.into())).expect("database seed"),
        format!("/?view=reader&seedDbKey={seed}")
    );
    assert_eq!(
        reader_window_path(Some(seed.into()), None).expect("legacy seed"),
        format!("/?view=reader&seedKey={seed}")
    );
    for (seed_key, seed_db_key) in [
        (None, None),
        (Some(seed.to_string()), Some(seed.to_string())),
        (Some("not-a-uuid".to_string()), None),
        (None, Some("not-a-uuid".to_string())),
    ] {
        assert!(
            reader_window_path(seed_key, seed_db_key).is_err(),
            "Reader URLs must carry exactly one valid opaque seed"
        );
    }
}

#[test]
fn reader_window_labels_match_the_lowercase_typescript_digest_contract() {
    let canonical = "reader-0123456789abcdef0123456789abcdef";
    assert!(is_reader_window_label(canonical));
    assert!(
        !is_reader_window_label("reader-0123456789ABCDEF0123456789abcdef"),
        "TypeScript emits lowercase hexadecimal SHA-256 bytes only"
    );
}

#[test]
fn dispatcher_accepts_a_database_reader_seed_but_rejects_two_seeds() {
    let app = app();
    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .expect("main window");
    let label = "reader-0123456789abcdef0123456789abcdef";
    let seed = "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6";

    get_ipc_response(
        &main,
        request_with_body(
            "open_reader_window",
            "tauri://localhost",
            serde_json::json!({ "label": label, "seedKey": null, "seedDbKey": seed }),
        ),
    )
    .expect("main may open a Reader with one opaque IndexedDB seed");

    let ambiguous = rejection(get_ipc_response(
        &main,
        request_with_body(
            "open_reader_window",
            "tauri://localhost",
            serde_json::json!({ "label": "reader-fedcba9876543210fedcba9876543210", "seedKey": seed, "seedDbKey": seed }),
        ),
    ));
    assert!(
        ambiguous.contains("exactly one target seed key"),
        "the dispatcher must reject ambiguous Reader handoffs: {ambiguous}"
    );
}
