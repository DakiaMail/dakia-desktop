//! Artifact-bound attachment acceptance through the production Tauri commands.
//!
//! This module is compiled only with the explicit fictional-mail fixture
//! feature. The orchestrator starts a TLS loopback IMAP transcript and creates
//! a sentinel-marked temporary profile and Downloads directory. The test then
//! parses the provider MIME through `MailService`, persists its Reader cache,
//! dispatches the real Tauri commands, and exports their IPC results for the
//! Reader DOM regression.

use super::*;
use serde_json::{json, Value};
use std::{
    fs,
    sync::{Mutex as StdMutex, OnceLock},
};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};

const UID_VALIDITY: u32 = 77;
const UID: u32 = 42;
const UNSAFE_BYTES: &[u8] = b"fictional attachment bytes\0\xff";

static BULK_EPOCH_AFTER_FIRST_SAVE: OnceLock<StdMutex<Option<u32>>> = OnceLock::new();

fn arm_stale_epoch_after_first_save(uid_validity: u32) {
    let mut armed = BULK_EPOCH_AFTER_FIRST_SAVE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .expect("stale-epoch fixture lock");
    assert!(
        armed.replace(uid_validity).is_none(),
        "stale-epoch fixture hook must be one-shot"
    );
}

pub(super) async fn after_successful_bulk_save_for_test(
    store: &Store,
    target: &MailSummary,
    saved_count: usize,
) {
    if saved_count != 1 {
        return;
    }
    let Some(uid_validity) = BULK_EPOCH_AFTER_FIRST_SAVE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .expect("stale-epoch fixture lock")
        .take()
    else {
        return;
    };
    store
        .save_mailbox_catalog_state(
            Uuid::parse_str(&target.account_id).expect("fixture message account UUID"),
            &target.mailbox,
            &target.mailbox,
            uid_validity,
            1,
            true,
        )
        .await
        .expect("fixture changes mailbox UIDVALIDITY between real saves");
}

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("the artifact lane must use the persisted provider MIME cache")
    }
}

fn request(command: &str, body: Value) -> InvokeRequest {
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

fn response_json(body: InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(value) => serde_json::from_str(&value).expect("IPC JSON response"),
        InvokeResponseBody::Raw(_) => panic!("attachment command returned raw IPC bytes"),
    }
}

fn response_string(body: InvokeResponseBody) -> String {
    response_json(body)
        .as_str()
        .expect("attachment save response string")
        .to_owned()
}

fn app_state(store: Store, data_dir: PathBuf) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir,
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "attachment-command-artifact".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

#[cfg(unix)]
fn assert_private_nonexecutable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .expect("saved attachment metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "saved attachment is private");
    assert_eq!(mode & 0o111, 0, "saved attachment is never executable");
}

#[cfg(not(unix))]
fn assert_private_nonexecutable(_path: &Path) {}

#[cfg(target_os = "macos")]
fn assert_quarantined(path: &Path) {
    use std::ffi::CString;
    let path = CString::new(path.as_os_str().as_encoded_bytes()).expect("path has no NUL");
    let name = CString::new("com.apple.quarantine").unwrap();
    let size =
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    assert!(
        size > 0,
        "email attachment must receive a macOS quarantine xattr"
    );
}

#[cfg(not(target_os = "macos"))]
fn assert_quarantined(_path: &Path) {}

#[test]
fn provider_mime_to_tauri_attachment_commands_exports_reader_artifact() {
    let Some(root) = std::env::var_os("DAKIA_ATTACHMENT_COMMAND_ARTIFACT_ROOT") else {
        // The script owns the real TLS server and explicit fixture feature.
        // A normal source run still compiles this command-boundary test.
        return;
    };
    let root = PathBuf::from(root)
        .canonicalize()
        .expect("artifact root exists");
    let nonce = std::env::var("DAKIA_ATTACHMENT_COMMAND_ARTIFACT_NONCE")
        .expect("artifact run supplies nonce");
    assert_eq!(
        fs::read_to_string(root.join(".dakia-attachment-command-artifact"))
            .expect("artifact ownership marker"),
        format!("{nonce}\n")
    );

    let fixture = NativeMailSafetyFixture::from_environment()
        .expect("read explicit fixture environment")
        .expect("artifact test requires the native-mail-safety fixture");
    let store = tauri::async_runtime::block_on(Store::open(fixture.data_dir.join("dakia.db")))
        .expect("open owned fixture profile");
    tauri::async_runtime::block_on(async {
        fixture.seed(&store).await.expect("seed fictional account");
        store
            .save_mailbox_catalog_state(fixture.account.id, "INBOX", "INBOX", UID_VALIDITY, 1, true)
            .await
            .expect("save fixture mailbox epoch");
    });

    // This is the first link in the artifact: real provider MIME, parsed by
    // the normal complete-message path, then persisted as ordinary Reader
    // metadata. Attachment bytes intentionally remain provider-only.
    let message = tauri::async_runtime::block_on(async {
        let fetched = MailService::new(store.clone())
            .fetch_full_message(&fixture.account, "INBOX", UID)
            .await
            .expect("fictional provider MIME fetch");
        assert_eq!(fetched.mailbox_uid_validity, Some(i64::from(UID_VALIDITY)));
        assert_eq!(fetched.attachments.len(), 2, "fixture has two downloads");
        assert!(fetched.attachments[0].attachment.is_potentially_unsafe);
        assert_eq!(
            fetched.attachments[0].attachment.filename, "quarterly-report.pdf.command",
            "MIME traversal name is sanitized before it reaches the Reader"
        );
        store
            .upsert_messages(std::slice::from_ref(&fetched))
            .await
            .expect("persist parsed provider message");
        store
            .cache_message_content(
                &fetched.id,
                fetched.is_flagged,
                CachedMessageContent {
                    body_text: fetched.body_text.clone(),
                    body_html: fetched.body_html.clone(),
                    unsubscribe_kind: fetched.unsubscribe_kind.clone(),
                    attachments: fetched
                        .attachments
                        .iter()
                        .map(|item| item.attachment.clone())
                        .collect(),
                },
            )
            .await
            .expect("persist Reader attachment metadata");
        fetched
    });

    let state = app_state(store.clone(), fixture.data_dir.clone());
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            message_content,
            save_attachment,
            save_all_attachments,
        ])
        .build(super::test_support::mock_context())
        .expect("build production Tauri command dispatcher");
    let reader = WebviewWindowBuilder::new(
        &app,
        "reader-attachment-command-artifact",
        WebviewUrl::default(),
    )
    .build()
    .expect("build Reader-capability scoped window");

    let content = response_json(
        get_ipc_response(
            &reader,
            request(
                "message_content",
                json!({
                    "messageId": message.id,
                    "expectedUidValidity": UID_VALIDITY,
                }),
            ),
        )
        .expect("Reader may load persisted provider attachment metadata"),
    );
    assert_eq!(content["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(
        content["attachments"][0]["is_potentially_unsafe"], true,
        "the real Tauri projection keeps the unsafe warning"
    );

    let unsafe_attachment = &message.attachments[0].attachment;
    let saved_path = response_string(
        get_ipc_response(
            &reader,
            request(
                "save_attachment",
                json!({
                    "messageId": message.id,
                    "attachmentId": unsafe_attachment.id,
                    "expectedUidValidity": UID_VALIDITY,
                }),
            ),
        )
        .expect("individual attachment command succeeds"),
    );
    let saved_path = PathBuf::from(&saved_path);
    assert!(
        saved_path.starts_with(&root),
        "fixture save remains contained"
    );
    assert_eq!(
        fs::read(&saved_path).expect("read individual save"),
        UNSAFE_BYTES
    );
    assert_private_nonexecutable(&saved_path);
    assert_quarantined(&saved_path);

    // First prove the normal successful bulk command returns both saved paths
    // and preserves both opaque payloads. The later run below is deliberately
    // separate so a truthful partial outcome cannot hide this success case.
    let successful_bulk = response_json(
        get_ipc_response(
            &reader,
            request(
                "save_all_attachments",
                json!({
                    "messageId": message.id,
                    "expectedUidValidity": UID_VALIDITY,
                }),
            ),
        )
        .expect("successful Save all command returns both paths"),
    );
    let successful_bulk = successful_bulk
        .as_array()
        .expect("Save all response is a path array")
        .iter()
        .map(|value| PathBuf::from(value.as_str().expect("saved attachment path")))
        .collect::<Vec<_>>();
    assert_eq!(successful_bulk.len(), 2);
    let successful_bytes = successful_bulk
        .iter()
        .map(|path| {
            assert_private_nonexecutable(path);
            assert_quarantined(path);
            fs::read(path).expect("read successful bulk save")
        })
        .collect::<Vec<_>>();
    assert!(successful_bytes.contains(&UNSAFE_BYTES.to_vec()));
    assert!(successful_bytes.contains(&b"fictional safe attachment bytes\n".to_vec()));

    // Preserve the production stale-identity boundary independently from the
    // native UI's fixture-only partial-save marker below. A remote mailbox
    // epoch change after the first real write must prevent the second write.
    arm_stale_epoch_after_first_save(UID_VALIDITY + 1);
    let stale_epoch_partial = get_ipc_response(
        &reader,
        request(
            "save_all_attachments",
            json!({
                "messageId": message.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
    )
    .expect_err("changed UIDVALIDITY returns a partial save after one publish");
    let stale_epoch_partial = stale_epoch_partial
        .as_str()
        .map(|value| serde_json::from_str(value).expect("partial save error JSON"))
        .unwrap_or(stale_epoch_partial);
    assert_eq!(stale_epoch_partial["kind"], "partial");
    assert_eq!(stale_epoch_partial["savedCount"], 1);
    assert_eq!(stale_epoch_partial["totalCount"], 2);
    tauri::async_runtime::block_on(store.save_mailbox_catalog_state(
        fixture.account.id,
        "INBOX",
        "INBOX",
        UID_VALIDITY,
        1,
        true,
    ))
    .expect("restore fixture mailbox epoch for the independent native marker case");

    // The fixture-only marker is consumed after this command's first genuine
    // save. The next normal batch step reports 1/2 truthfully and does not
    // attempt to publish a second successful file.
    attachment_partial_fault::arm_partial_after_first_save()
        .expect("arm the owned one-shot partial-save fixture marker");
    let partial = get_ipc_response(
        &reader,
        request(
            "save_all_attachments",
            json!({
                "messageId": message.id,
                "expectedUidValidity": UID_VALIDITY,
            }),
        ),
    )
    .expect_err("fixture marker returns partial after one genuine save");
    let partial = partial
        .as_str()
        .map(|value| serde_json::from_str(value).expect("partial save error JSON"))
        .unwrap_or(partial);
    assert_eq!(partial["kind"], "partial");
    assert_eq!(partial["savedCount"], 1);
    assert_eq!(partial["totalCount"], 2);

    let downloads = PathBuf::from(
        std::env::var("DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR")
            .expect("fixture Downloads directory"),
    );
    let saved_files = fs::read_dir(&downloads)
        .expect("read fixture Downloads")
        .map(|entry| entry.expect("saved entry").path())
        .collect::<Vec<_>>();
    assert_eq!(
        saved_files.len(),
        5,
        "individual, successful bulk, stale-epoch partial, and fixture partial saves"
    );
    let all_saved_bytes = saved_files
        .iter()
        .map(|path| {
            assert_private_nonexecutable(path);
            assert_quarantined(path);
            fs::read(path).expect("read published attachment")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        all_saved_bytes
            .iter()
            .filter(|bytes| bytes.as_slice() == UNSAFE_BYTES)
            .count(),
        4,
        "individual, successful bulk, and both partial paths preserve the unsafe bytes"
    );
    assert_eq!(
        all_saved_bytes
            .iter()
            .filter(|bytes| bytes.as_slice() == b"fictional safe attachment bytes\n")
            .count(),
        1,
        "only the successful bulk command saves the second attachment"
    );

    let artifact_path =
        std::env::var_os("DAKIA_ATTACHMENT_COMMAND_TAURI_ARTIFACT").expect("artifact output path");
    fs::write(
        artifact_path,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "nonce": nonce,
            "message": message,
            "content": content,
            "individual": {
                "path": saved_path,
                "bytesBase64": STANDARD.encode(UNSAFE_BYTES),
                "unsafe": true,
            },
            "bulk": {
                "successfulPaths": successful_bulk,
                "staleEpochError": stale_epoch_partial,
                "error": partial,
                "partialPublishedCount": 1,
            },
        }))
        .expect("serialize attachment command artifact"),
    )
    .expect("write actual Tauri attachment command artifact");
}
