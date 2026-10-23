//! Security regressions at the native command and filesystem boundaries.
use super::*;
use dakia_core::{AttachmentSizeKind, SendFailure, SendFailureKind};
use tempfile::tempdir;

#[test]
fn send_command_preserves_delivery_outcome_without_echoing_server_secrets() {
    for kind in [
        SendFailureKind::NotSubmitted,
        SendFailureKind::DeliveryUncertain,
        SendFailureKind::SentCopyFailed,
    ] {
        let failure = anyhow::Error::new(SendFailure {
            kind,
            message: "535 password=secret-canary body=private-mail-canary".into(),
        })
        .context("untrusted provider detail");
        let encoded = send_command_error(failure);
        let decoded: SendFailure = serde_json::from_str(&encoded).expect("typed IPC error");
        assert_eq!(decoded.kind, kind);
        assert!(!encoded.contains("canary"));
        assert!(!encoded.contains("535"));
        if kind == SendFailureKind::SentCopyFailed {
            assert!(decoded.message.contains("Do not send it again"));
        }
    }
    let encoded = send_command_error(anyhow::anyhow!("internal-secret-canary"));
    assert!(!encoded.contains("canary"));
    assert_eq!(
        serde_json::from_str::<SendFailure>(&encoded).unwrap().kind,
        SendFailureKind::DeliveryUncertain
    );
}

#[test]
fn native_link_boundary_rejects_active_schemes_and_ambiguous_inputs() {
    for url in [
        "https://example.test/a?q=%E7%8C%AB#part",
        "HTTP://example.test/",
        "mailto:person@example.test?subject=Hello%20there",
    ] {
        assert!(validate_external_url(url).is_ok(), "{url:?}");
    }
    for url in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,hello",
        "blob:https://example.test/123",
        "asset://localhost/private",
        "//example.test/",
        "https://user:password@example.test/",
        "https://trusted.test@evil.test/",
        " https://example.test/",
        "https://example.test/\n",
        "ht\ttps://example.test/",
        "https://example.test/\0payload",
        "not a URL",
    ] {
        assert!(validate_external_url(url).is_err(), "{url:?}");
    }
}

#[test]
fn unsafe_download_metadata_and_bytes_survive_native_serialization_and_save() {
    let directory = tempdir().unwrap();
    let attachment = Attachment {
        id: "unsafe-attachment".into(),
        message_id: "fictional-message".into(),
        filename: "../../invoice.猫.command".into(),
        mime_type: "application/octet-stream".into(),
        size_bytes: 9,
        size_kind: AttachmentSizeKind::Decoded,
        is_inline: false,
        presentation: dakia_core::AttachmentPresentation::Downloadable,
        is_potentially_unsafe: true,
    };
    let serialized = serde_json::to_value(&attachment).unwrap();
    assert_eq!(serialized["is_potentially_unsafe"], true);
    assert!(
        is_downloadable_attachment(&attachment),
        "warnings do not silently hide downloads"
    );
    let filename = dakia_core::mail::safe_attachment_filename(&attachment.filename, 0);
    let bytes = b"\0\xffpayload";
    let path = save_private_download(directory.path(), &filename, bytes).unwrap();
    assert_eq!(path.parent(), Some(directory.path()));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(&path).unwrap();
        let mut value = [0_u8; 256];
        let length = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert!(
            length > 0,
            "email downloads must keep quarantine provenance"
        );
        assert!(std::str::from_utf8(&value[..length as usize])
            .unwrap()
            .contains(";Dakia;"));
    }
}

#[cfg(unix)]
#[test]
fn attachment_collision_never_follows_an_existing_symlink() {
    use std::os::unix::fs::symlink;
    let directory = tempdir().unwrap();
    let target = directory.path().join("private-target");
    std::fs::write(&target, b"preserve me").unwrap();
    symlink(&target, directory.path().join("invoice.command")).unwrap();
    let path = save_private_download(directory.path(), "invoice.command", b"download").unwrap();
    assert_eq!(path.file_name().unwrap(), "invoice (1).command");
    assert_eq!(std::fs::read(target).unwrap(), b"preserve me");
    assert_eq!(std::fs::read(path).unwrap(), b"download");
}

async fn send_fixture(store: &Store) -> ComposeMessage {
    let draft: AccountDraft = serde_json::from_value(serde_json::json!({
        "email": "send-journal@example.test",
        "display_name": "Fictional sender",
        "provider_id": "custom"
    }))
    .unwrap();
    let account = draft.into_account(provider::by_id("custom").unwrap());
    store.save_account(&account).await.unwrap();
    serde_json::from_value(serde_json::json!({
        "account_id": account.id,
        "to": ["recipient@example.test"],
        "subject": "Fictional submission",
        "body_text": "No real mail is sent by this test."
    }))
    .unwrap()
}

#[tokio::test]
async fn native_send_attempt_submits_once_across_windows_and_reopen() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("dakia.db");
    let store = Store::open(&path).await.unwrap();
    let other_window = Store::open(&path).await.unwrap();
    let draft = send_fixture(&store).await;
    let attempt = Uuid::new_v4();
    let submissions = Arc::new(AtomicUsize::new(0));
    let (entered, first_is_pending) = tokio::sync::oneshot::channel();
    let (release, resume_first) = tokio::sync::oneshot::channel();
    let first_store = store.clone();
    let first_draft = draft.clone();
    let first_submissions = submissions.clone();
    let first = tokio::spawn(async move {
        submit_send_attempt(&first_store, &first_draft, Some(attempt), async {
            first_submissions.fetch_add(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            resume_first.await.unwrap();
            Ok("accepted".into())
        })
        .await
    });
    // Keep SMTP in flight until the other window has checked the durable
    // Pending claim. A scheduler yield does not establish this ordering.
    tokio::time::timeout(std::time::Duration::from_secs(5), first_is_pending)
        .await
        .expect("first submission reaches its deterministic barrier")
        .unwrap();
    let second = submit_send_attempt(&other_window, &draft, Some(attempt), async {
        submissions.fetch_add(1, Ordering::SeqCst);
        Ok("accepted".into())
    })
    .await;
    release.send(()).unwrap();
    assert!(first.await.unwrap().is_ok());
    assert_eq!(submissions.load(Ordering::SeqCst), 1);
    let second_failure: SendFailure = serde_json::from_str(&second.unwrap_err()).unwrap();
    assert_eq!(second_failure.kind, SendFailureKind::DeliveryUncertain);
    drop(other_window);
    drop(store);
    let reopened = Store::open(path).await.unwrap();
    assert!(
        submit_send_attempt(&reopened, &draft, Some(attempt), async {
            panic!("a persisted submission must never poll SMTP twice");
        })
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn native_send_releases_only_known_not_submitted_attempts() {
    let store = Store::in_memory().await.unwrap();
    let draft = send_fixture(&store).await;
    for kind in [
        SendFailureKind::NotSubmitted,
        SendFailureKind::DeliveryUncertain,
        SendFailureKind::SentCopyFailed,
    ] {
        let attempt = Uuid::new_v4();
        let first = submit_send_attempt(&store, &draft, Some(attempt), async {
            Err(anyhow::Error::new(SendFailure {
                kind,
                message: "server-secret-canary".into(),
            }))
        })
        .await
        .unwrap_err();
        assert!(!first.contains("canary"));
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let retry = submit_send_attempt(&store, &draft, Some(attempt), async {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok("accepted".into())
        })
        .await;
        if kind == SendFailureKind::NotSubmitted {
            assert!(retry.is_ok());
            assert_eq!(polls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            let failure: SendFailure = serde_json::from_str(&retry.unwrap_err()).unwrap();
            assert_eq!(failure.kind, kind);
        }
    }
}

#[tokio::test]
async fn native_send_changed_payload_cannot_reuse_a_submitted_attempt() {
    let store = Store::in_memory().await.unwrap();
    let mut draft = send_fixture(&store).await;
    let attempt = Uuid::new_v4();
    submit_send_attempt(&store, &draft, Some(attempt), async {
        Ok("accepted".into())
    })
    .await
    .unwrap();
    draft.subject = "Changed subject".into();
    let rejected = submit_send_attempt(&store, &draft, Some(attempt), async {
        panic!("changed payload must not bypass send identity");
    })
    .await
    .unwrap_err();
    assert_eq!(
        serde_json::from_str::<SendFailure>(&rejected).unwrap().kind,
        SendFailureKind::DeliveryUncertain
    );
}

#[tokio::test]
async fn native_mail_target_rejects_recycled_uids_even_when_local_ids_are_identical() {
    let store = Store::in_memory().await.unwrap();
    let draft = send_fixture(&store).await;
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../testdata/tauri-contracts/high-risk.json"
    ))
    .unwrap();
    let mut message: MailSummary =
        serde_json::from_value(fixtures["events"]["mailArrived"]["messages"][0].clone()).unwrap();
    message.account_id = draft.account_id.to_string();
    store.upsert_messages(&[message.clone()]).await.unwrap();
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();
    let selected = require_current_message_target(
        &store,
        draft.account_id,
        "INBOX",
        42,
        Some(&message.id),
        Some(77),
    )
    .await
    .unwrap();
    assert_eq!(selected.mailbox_uid_validity, Some(77));

    // A replacement mailbox can reuse both UID and Dakia's stable local ID.
    // The UI selection must remain bound to epoch 77 rather than the new row.
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 78, 1, true)
        .await
        .unwrap();
    message.subject = "A different email with a recycled UID".into();
    store.upsert_messages(&[message.clone()]).await.unwrap();
    assert!(require_current_message_target(
        &store,
        draft.account_id,
        "INBOX",
        42,
        Some(&message.id),
        selected.mailbox_uid_validity
    )
    .await
    .is_err());
    assert!(require_current_message_target(
        &store,
        draft.account_id,
        "INBOX",
        42,
        Some(&message.id),
        Some(78)
    )
    .await
    .is_ok());
    for (account, mailbox, uid, id, epoch) in [
        (
            Uuid::new_v4(),
            "INBOX",
            42,
            Some(message.id.as_str()),
            Some(78),
        ),
        (
            draft.account_id,
            "Trash",
            42,
            Some(message.id.as_str()),
            Some(78),
        ),
        (
            draft.account_id,
            "INBOX",
            43,
            Some(message.id.as_str()),
            Some(78),
        ),
        (draft.account_id, "INBOX", 42, None, Some(78)),
        (
            draft.account_id,
            "INBOX",
            42,
            Some(message.id.as_str()),
            None,
        ),
    ] {
        assert!(
            require_current_message_target(&store, account, mailbox, uid, id, epoch)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn native_content_cache_refuses_a_recycled_uidvalidity_target() {
    let store = Store::in_memory().await.unwrap();
    let draft = send_fixture(&store).await;
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../testdata/tauri-contracts/high-risk.json"
    ))
    .unwrap();
    let mut message: MailSummary =
        serde_json::from_value(fixtures["events"]["mailArrived"]["messages"][0].clone()).unwrap();
    message.account_id = draft.account_id.to_string();
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();
    store.upsert_messages(&[message.clone()]).await.unwrap();
    let target = current_message_at_epoch(&store, &message.id, Some(77))
        .await
        .unwrap();
    store
        .cache_message_content(
            &message.id,
            false,
            CachedMessageContent {
                body_text: "old body must never leak".into(),
                body_html: None,
                unsubscribe_kind: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    // IMAP may reuse the same UID and stable local ID after UIDVALIDITY rolls.
    // The old cache is therefore not an acceptable response for the new row.
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 78, 1, true)
        .await
        .unwrap();
    message.subject = "replacement message at the same UID".into();
    store.upsert_messages(&[message]).await.unwrap();

    assert!(confirm_message_epoch(&store, &target).await.is_err());
    assert!(cached_message_content(&store, &target).await.is_err());
    assert!(current_message_at_epoch(&store, &target.id, None)
        .await
        .is_err());
}

#[tokio::test]
async fn native_unsubscribe_rejects_a_recycled_uidvalidity_before_any_action() {
    let store = Store::in_memory().await.unwrap();
    let draft = send_fixture(&store).await;
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../testdata/tauri-contracts/high-risk.json"
    ))
    .unwrap();
    let mut message: MailSummary =
        serde_json::from_value(fixtures["events"]["mailArrived"]["messages"][0].clone()).unwrap();
    message.account_id = draft.account_id.to_string();
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();
    store.upsert_messages(&[message.clone()]).await.unwrap();
    let selected = current_message_at_epoch(&store, &message.id, Some(77))
        .await
        .unwrap();

    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 78, 1, true)
        .await
        .unwrap();
    message.unsubscribe_kind = Some("one_click".into());
    message.unsubscribe_url = Some("https://unsubscribe.example.test/replace-me".into());
    store.upsert_messages(&[message]).await.unwrap();

    assert!(
        current_message_at_epoch(&store, &selected.id, selected.mailbox_uid_validity)
            .await
            .is_err()
    );
    assert!(confirm_message_epoch(&store, &selected).await.is_err());
}

#[tokio::test]
async fn ai_message_targets_reject_recycled_uidvalidity_before_hydration_or_transmission() {
    let store = Store::in_memory().await.unwrap();
    let draft = send_fixture(&store).await;
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../testdata/tauri-contracts/high-risk.json"
    ))
    .unwrap();
    let mut message: MailSummary =
        serde_json::from_value(fixtures["events"]["mailArrived"]["messages"][0].clone()).unwrap();
    message.account_id = draft.account_id.to_string();
    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();
    store.upsert_messages(&[message.clone()]).await.unwrap();
    let stale_targets = vec![AiMessageTarget {
        message_id: message.id.clone(),
        expected_uid_validity: Some(77),
    }];

    store
        .save_mailbox_catalog_state(draft.account_id, "INBOX", "INBOX", 78, 1, true)
        .await
        .unwrap();
    message.subject = "replacement must not reach AI".into();
    store.upsert_messages(&[message]).await.unwrap();

    assert!(confirm_ai_message_targets(&store, &stale_targets)
        .await
        .is_err());
    assert!(required_ai_message_targets(&AiInput {
        provider: "local".into(),
        base_url: None,
        model: "fictional".into(),
        api_key: None,
        executable: None,
        model_path: None,
        message_ids: vec![stale_targets[0].message_id.clone()],
        message_targets: vec![AiMessageTarget {
            message_id: stale_targets[0].message_id.clone(),
            expected_uid_validity: None,
        }],
        instruction: None,
    })
    .is_err());
}
