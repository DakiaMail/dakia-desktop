use crate::realtime::{MailArrival, MailHydrated, RealtimeSyncStatus};
use crate::{MessageContent, MessageContentCommandError, MessageContentErrorKind};
use chrono::{DateTime, Utc};
use dakia_core::{Attachment, AttachmentPresentation, MailSummary};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../testdata/tauri-contracts/high-risk.json"
    )))
    .expect("Tauri contract fixture must be valid JSON")
}

fn fixture_message() -> MailSummary {
    MailSummary {
        id: "message-01".into(),
        account_id: "0f452e8d-8d11-4643-944c-0e9f8f243311".into(),
        mailbox: "INBOX".into(),
        uid: 42,
        message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "thread-01".into(),
        subject: "Quarterly update".into(),
        from_name: Some("Mara Example".into()),
        from_address: "mara@example.test".into(),
        to_addresses: "alex@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: DateTime::parse_from_rfc3339("2026-07-30T09:15:00Z")
            .expect("fixture time must be RFC 3339")
            .with_timezone(&Utc),
        snippet: "Quarterly update is ready.".into(),
        body_text: "Quarterly update is ready.".into(),
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

#[test]
fn message_content_success_and_error_envelopes_match_the_shared_fixture() {
    let fixture = fixture();
    let content = MessageContent {
        body_text: "Quarterly update is ready.".into(),
        body_html: None,
        unsubscribe_kind: Some("web".into()),
        attachments: vec![Attachment {
            id: "attachment-01".into(),
            message_id: "message-01".into(),
            filename: "update.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            is_inline: false,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: false,
        }],
    };

    assert_eq!(
        serde_json::to_value(content).expect("message content must serialize for IPC"),
        fixture["messageContent"]["success"]
    );
    assert_eq!(
        serde_json::to_value(MessageContentCommandError {
            kind: MessageContentErrorKind::ResourceLimit,
        })
        .expect("message-content error must serialize for IPC"),
        fixture["messageContent"]["error"]
    );
    for (index, kind) in [
        MessageContentErrorKind::ResourceLimit,
        MessageContentErrorKind::Malformed,
        MessageContentErrorKind::Undecodable,
        MessageContentErrorKind::Unsupported,
        MessageContentErrorKind::Transient,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            serde_json::to_value(MessageContentCommandError { kind })
                .expect("message-content error variant must serialize"),
            fixture["messageContent"]["errorVariants"][index]
        );
    }
}

#[test]
fn provider_signature_inline_message_content_matches_the_shared_fixture() {
    // provider-signature-inline is parsed from raw MIME into this shared contract
    // by the core regression test before this native IPC serialization check.
    let fixture = fixture();
    assert_eq!(
        fixture["realisticFixtureIds"]["providerSignature"],
        "provider-signature-inline"
    );
    let content = MessageContent {
        body_text: fixture["messageContent"]["providerSignature"]["body_text"]
            .as_str()
            .expect("provider body text must be a string")
            .into(),
        body_html: Some(
            fixture["messageContent"]["providerSignature"]["body_html"]
                .as_str()
                .expect("provider HTML must be a string")
                .into(),
        ),
        unsubscribe_kind: None,
        attachments: vec![Attachment {
            id: "provider-signature-pdf".into(),
            message_id: "message-provider-signature".into(),
            filename: "claim-documents.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 3,
            is_inline: false,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: false,
        }],
    };

    assert_eq!(
        serde_json::to_value(content).expect("provider content must serialize for IPC"),
        fixture["messageContent"]["providerSignature"]
    );
}

#[test]
fn mail_event_payloads_match_the_shared_fixture_casing_uuid_times_and_nulls() {
    let fixture = fixture();
    let account_id = Uuid::parse_str("0f452e8d-8d11-4643-944c-0e9f8f243311")
        .expect("fixture account ID must be a UUID");
    let arrival = MailArrival {
        event_id: Uuid::parse_str("6e84311f-ff3a-4cbd-a037-a5bedc79f2f2")
            .expect("fixture event ID must be a UUID"),
        account_id,
        messages: vec![fixture_message()],
        detected_at: "2026-07-30T09:15:03Z".into(),
    };
    let hydrated = MailHydrated {
        account_id,
        message_id: "message-01".into(),
    };
    let idle = RealtimeSyncStatus {
        account_id,
        state: "idle".into(),
        retry_at: None,
        error_kind: None,
    };
    let retrying = RealtimeSyncStatus {
        account_id,
        state: "retrying".into(),
        retry_at: Some("2026-07-30T09:20:00Z".into()),
        error_kind: Some("connection".into()),
    };

    assert_eq!(
        serde_json::to_value(arrival).expect("mail-arrived payload must serialize"),
        fixture["events"]["mailArrived"]
    );
    assert_eq!(
        serde_json::to_value(hydrated).expect("mail-hydrated payload must serialize"),
        fixture["events"]["mailHydrated"]
    );
    assert_eq!(
        json!({ "accountId": account_id }),
        fixture["events"]["mailChanged"]
    );
    assert_eq!(
        serde_json::to_value(idle).expect("idle sync state must serialize"),
        fixture["events"]["mailSyncStateWithNulls"]
    );
    assert_eq!(
        serde_json::to_value(retrying).expect("retrying sync state must serialize"),
        fixture["events"]["mailSyncStateRetrying"]
    );
}
