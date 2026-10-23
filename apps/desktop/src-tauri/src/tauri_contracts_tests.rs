use crate::realtime::{MailArrival, MailHydrated, RealtimeSyncStatus};
use crate::{
    AiInput, MessageContent, MessageContentCommandError, MessageContentErrorKind,
    RemoteSearchResult,
};
use chrono::{DateTime, Utc};
use dakia_core::{
    Attachment, AttachmentPresentation, AttachmentSizeKind, MailSummary, SearchQuery,
};
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
        mailbox_uid_validity: None,
        id: "message-01".into(),
        account_id: "0f452e8d-8d11-4643-944c-0e9f8f243311".into(),
        mailbox: "INBOX".into(),
        uid: 42,
        message_id: None,
        gmail_message_id: None,
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

fn late_unsafe_attachment_message() -> MailSummary {
    MailSummary {
        mailbox_uid_validity: Some(424242),
        id: "message-unsafe-attachment".into(),
        account_id: "3a2a2c45-4d19-4a53-ae1d-92671c3f43c2".into(),
        mailbox: "INBOX".into(),
        uid: 77,
        message_id: Some("<unsafe-attachment@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "thread-unsafe-attachment".into(),
        subject: "Fictional attachment safety check".into(),
        from_name: Some("Fixture Sender".into()),
        from_address: "fixture-sender@example.test".into(),
        to_addresses: "fixture-recipient@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: DateTime::parse_from_rfc3339("2026-07-30T09:21:00Z")
            .expect("late fixture time must be RFC 3339")
            .with_timezone(&Utc),
        snippet: "A fictional executable attachment is included.".into(),
        body_text: String::new(),
        body_html: None,
        content_state: "headers_only".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: false,
        is_flagged: false,
        has_attachments: true,
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn remote_search_response_and_query_match_the_shared_fixture() {
    let fixture = fixture();
    let query: SearchQuery =
        serde_json::from_value(fixture["commands"]["searchRemote"]["arguments"]["query"].clone())
            .expect("search query must deserialize through the native command type");
    assert_eq!(query.text, "remote fixture query");
    assert_eq!(
        query.account_ids,
        [Uuid::parse_str("0f452e8d-8d11-4643-944c-0e9f8f243311")
            .expect("fixture account ID must be a UUID")]
    );
    assert_eq!(query.mailbox.as_deref(), Some("INBOX"));
    assert_eq!(query.limit, Some(500));

    let response = RemoteSearchResult {
        messages: vec![fixture_message()],
        limit_reached: true,
    };
    assert_eq!(
        serde_json::to_value(response).expect("remote search response must serialize for IPC"),
        fixture["remoteSearch"]["success"]
    );
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
            size_kind: AttachmentSizeKind::Decoded,
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
            size_kind: AttachmentSizeKind::Decoded,
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
fn unsafe_attachment_warning_metadata_survives_the_native_ipc_fixture() {
    let fixture = fixture();
    let content = MessageContent {
        body_text: fixture["messageContent"]["unsafeAttachment"]["body_text"]
            .as_str()
            .expect("unsafe attachment body must be a string")
            .into(),
        body_html: None,
        unsubscribe_kind: None,
        attachments: vec![Attachment {
            id: "unsafe-attachment-01".into(),
            message_id: "message-unsafe-attachment".into(),
            filename: "invoice.猫.command".into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 9,
            size_kind: AttachmentSizeKind::Decoded,
            is_inline: false,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: true,
        }],
    };

    assert_eq!(
        serde_json::to_value(content).expect("unsafe attachment content must serialize"),
        fixture["messageContent"]["unsafeAttachment"]
    );
}

#[test]
fn high_risk_message_commands_carry_the_captured_uidvalidity() {
    let fixture = fixture();
    for command in [
        "messageContent",
        "messageAttachments",
        "hydrateMessage",
        "saveAttachment",
        "saveAllAttachments",
        "exportMessage",
        "forwardAttachments",
        "unsubscribeMessage",
        "setMessageCategory",
    ] {
        let arguments = &fixture["commands"][command]["arguments"];
        assert_eq!(
            arguments["messageId"], "message-01",
            "{command} must retain its immutable message locator"
        );
        assert_eq!(
            arguments["expectedUidValidity"], 424242,
            "{command} must retain the captured mailbox epoch"
        );
    }
    assert_eq!(
        fixture["commands"]["saveAttachment"]["arguments"]["attachmentId"], "attachment-01",
        "the individual save command must retain its attachment target"
    );
}

#[test]
fn ai_inputs_deserialize_the_shared_immutable_message_targets() {
    let fixture = fixture();
    for command in ["aiSummarize", "aiDraft"] {
        let input: AiInput =
            serde_json::from_value(fixture["commands"][command]["arguments"]["input"].clone())
                .expect("fixture AI input must deserialize through the native command type");
        assert_eq!(input.provider, "local");
        assert_eq!(input.model, "fictional-local-model");
        assert_eq!(input.message_ids, ["message-01"]);
        assert_eq!(input.message_targets.len(), 1);
        assert_eq!(input.message_targets[0].message_id, "message-01");
        assert_eq!(input.message_targets[0].expected_uid_validity, Some(424242));
    }
    assert_eq!(
        fixture["commands"]["aiDraft"]["arguments"]["input"]["instruction"],
        "Write a brief, factual reply."
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

#[test]
fn removed_account_then_late_arrival_sequence_matches_the_shared_fixture() {
    let fixture = fixture();
    let account_id = Uuid::parse_str("3a2a2c45-4d19-4a53-ae1d-92671c3f43c2")
        .expect("late fixture account ID must be a UUID");
    let arrival = MailArrival {
        event_id: Uuid::parse_str("ee8b4d25-f8a7-4c47-9ba2-e96c583e7cd0")
            .expect("late fixture event ID must be a UUID"),
        account_id,
        messages: vec![late_unsafe_attachment_message()],
        detected_at: "2026-07-30T09:21:03Z".into(),
    };
    let sequence = json!([
        {
            "event": "account-removed",
            "payload": { "accountId": account_id },
        },
        {
            "event": "mail-arrived",
            "payload": serde_json::to_value(arrival)
                .expect("late mail-arrived payload must serialize"),
        },
    ]);

    assert_eq!(
        sequence,
        fixture["events"]["lateArrivalAfterAccountRemoval"],
        "the shared late-event sequence must remain byte-shape compatible with native serialization"
    );
}
