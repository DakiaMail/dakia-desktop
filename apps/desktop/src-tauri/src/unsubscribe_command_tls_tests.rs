//! Production unsubscribe IPC backed by an explicitly owned fictional TLS POST service.
//!
//! The Node runner supplies a sentinel-marked profile and a synthetic CA. This
//! module parses and persists a DKIM-signed raw message through core's real
//! parser, invokes the registered Tauri command through a Reader-scoped mock
//! window, and leaves exact HTTP receipt checks to the fixture process.

use super::*;
use dakia_core::native_mail_safety_unsubscribe_fixture::signed_one_click_message;
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};
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
        panic!("unsubscribe command acceptance must not run classification")
    }
}

fn request(body: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: "unsubscribe_message".into(),
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
        InvokeResponseBody::Json(value) => serde_json::from_str(&value).expect("JSON IPC body"),
        InvokeResponseBody::Raw(_) => panic!("unsubscribe command returned raw IPC bytes"),
    }
}

fn app_state(store: Store, data_dir: PathBuf) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir,
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "unsubscribe-command-tls-artifact".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn fixture_store() -> (NativeMailSafetyFixture, Store) {
    env::var_os("DAKIA_UNSUBSCRIBE_COMMAND_TLS_ROOT")
        .expect("requires isolated unsubscribe TLS fixture runner");
    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read native fixture environment")
        .expect("requires isolated unsubscribe TLS fixture runner");
    let store = tauri::async_runtime::block_on(Store::open(fixture.data_dir.join("dakia.db")))
        .expect("open owned fictional fixture database");
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
    });
    (fixture, store)
}

fn seed_signed_target(store: &Store, account: &Account, uid: u32) -> dakia_core::MailSummary {
    let transport = native_mail_safety_unsubscribe_transport()
        .expect("read unsubscribe command TLS transport")
        .expect("runner installs unsubscribe TLS transport");
    let mut message = tauri::async_runtime::block_on(signed_one_click_message(
        account,
        "INBOX",
        uid,
        transport.target_url(),
    ))
    .expect("parse signed raw message through fictional DKIM authority");
    message.mailbox_uid_validity = Some(EPOCH);
    tauri::async_runtime::block_on(store.upsert_messages(std::slice::from_ref(&message)))
        .expect("persist signed one-click message");
    message
}

fn reader(
    app: &tauri::App<tauri::test::MockRuntime>,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    WebviewWindowBuilder::new(app, "reader-unsubscribe-command-tls", WebviewUrl::default())
        .build()
        .expect("build Reader-scoped test window")
}

fn command_app(store: Store, data_dir: PathBuf) -> tauri::App<tauri::test::MockRuntime> {
    mock_builder()
        .manage(app_state(store, data_dir))
        .invoke_handler(tauri::generate_handler![unsubscribe_message])
        .build(super::test_support::mock_context())
        .expect("build production unsubscribe command dispatcher")
}

#[test]
#[ignore = "requires isolated unsubscribe TLS fixture runner"]
fn production_tauri_unsubscribe_posts_once_and_stale_or_wrong_account_never_reach_tls() {
    let (fixture, store) = fixture_store();
    let target = seed_signed_target(&store, &fixture.account, 41);
    let app = command_app(store.clone(), fixture.data_dir.clone());
    let reader = reader(&app);

    let stale = get_ipc_response(
        &reader,
        request(json!({
            "messageId": target.id,
            "expectedUidValidity": STALE_EPOCH,
        })),
    )
    .expect_err("stale epoch must reject before the TLS service");
    assert!(stale.to_string().contains("This message changed"));

    let mut wrong_account = seed_signed_target(&store, &fixture.account, 42);
    let absent_account = Uuid::new_v4();
    wrong_account.id = format!("{absent_account}:INBOX:42");
    wrong_account.account_id = absent_account.to_string();
    tauri::async_runtime::block_on(store.upsert_messages(std::slice::from_ref(&wrong_account)))
        .expect("persist wrong-account command target");
    let absent = get_ipc_response(
        &reader,
        request(json!({
            "messageId": wrong_account.id,
            "expectedUidValidity": EPOCH,
        })),
    )
    .expect_err("missing account must reject before the TLS service");
    assert!(
        absent.to_string().contains("This message changed"),
        "wrong-account target must fail before the TLS request: {absent}"
    );

    let result = response_json(
        get_ipc_response(
            &reader,
            request(json!({
                "messageId": target.id,
                "expectedUidValidity": EPOCH,
            })),
        )
        .expect("signed exact target posts through the production command"),
    );
    assert_eq!(result["kind"], "completed");

    let artifact = env::var_os("DAKIA_UNSUBSCRIBE_COMMAND_TLS_ARTIFACT")
        .expect("runner supplies command evidence path");
    fs::write(
        artifact,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "accountId": fixture.account.id,
            "targetId": target.id,
            "staleRejected": true,
            "wrongAccountRejected": true,
            "completed": true,
        }))
        .expect("serialize unsubscribe command evidence"),
    )
    .expect("write unsubscribe command evidence");
}

#[test]
#[ignore = "requires isolated unsubscribe TLS fixture runner"]
fn production_tauri_unsubscribe_rejects_untrusted_tls_without_http_post() {
    let (fixture, store) = fixture_store();
    let target = seed_signed_target(&store, &fixture.account, 43);
    let app = command_app(store, fixture.data_dir);
    let reader = reader(&app);
    let failure = get_ipc_response(
        &reader,
        request(json!({
            "messageId": target.id,
            "expectedUidValidity": EPOCH,
        })),
    )
    .expect_err("an untrusted TLS service must not produce an unsubscribe success");
    assert!(failure
        .to_string()
        .contains("one-click unsubscribe request failed"));
}

#[test]
#[ignore = "requires isolated unsubscribe TLS fixture runner"]
fn production_tauri_unsubscribe_rejects_wrong_tls_hostname_without_http_post() {
    let (fixture, store) = fixture_store();
    let target = seed_signed_target(&store, &fixture.account, 44);
    let app = command_app(store, fixture.data_dir);
    let reader = reader(&app);
    let failure = get_ipc_response(
        &reader,
        request(json!({
            "messageId": target.id,
            "expectedUidValidity": EPOCH,
        })),
    )
    .expect_err("a hostname-mismatched TLS service must not produce an unsubscribe success");
    assert!(failure
        .to_string()
        .contains("one-click unsubscribe request failed"));
}
