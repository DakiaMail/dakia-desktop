use super::*;

fn message(
    account_id: AccountId,
    mailbox: &str,
    uid: u32,
    thread_id: &str,
    received_offset: i64,
) -> MailSummary {
    MailSummary {
        id: stable_message_id(account_id, mailbox, uid),
        account_id: account_id.to_string(),
        mailbox: mailbox.into(),
        uid: i64::from(uid),
        mailbox_uid_validity: Some(71),
        message_id: Some(format!("<smart-{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: thread_id.into(),
        subject: format!("Fictional Smart message {uid}"),
        from_name: Some("Fictional Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: chrono::Utc::now() + chrono::Duration::seconds(received_offset),
        snippet: format!("preview {uid}"),
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

fn section<'a>(page: &'a SmartInboxPage, id: &str) -> &'a SmartInboxSectionPage {
    page.sections
        .iter()
        .find(|section| section.id == id)
        .unwrap_or_else(|| panic!("missing Smart section {id}"))
}

#[tokio::test]
async fn pending_classification_is_visible_in_other_without_crossing_scope_or_read_filters() {
    let store = Store::in_memory().await.unwrap();
    let selected_account = uuid::Uuid::new_v4();
    let other_account = uuid::Uuid::new_v4();
    let visible = message(selected_account, "INBOX", 1, "visible", 4);
    let mut already_read = message(selected_account, "INBOX", 2, "read", 3);
    already_read.is_read = true;
    let other_folder = message(selected_account, "Archive", 3, "archive", 2);
    let other_account_message = message(other_account, "INBOX", 4, "other-account", 1);
    store
        .upsert_messages(&[
            visible.clone(),
            already_read.clone(),
            other_folder,
            other_account_message,
        ])
        .await
        .unwrap();

    let page = store
        .search_smart_inbox(&SmartInboxQuery {
            account_ids: vec![selected_account],
            limit: Some(10),
        })
        .await
        .unwrap();

    assert_eq!(
        section(&page, "other")
            .conversations
            .iter()
            .map(|thread| thread.latest.id.as_str())
            .collect::<Vec<_>>(),
        [visible.id.as_str()]
    );
    assert_eq!(
        section(&page, "seen")
            .conversations
            .iter()
            .map(|thread| thread.latest.id.as_str())
            .collect::<Vec<_>>(),
        [already_read.id.as_str()]
    );
    assert_eq!(
        page.sections
            .iter()
            .flat_map(|section| &section.conversations)
            .filter(|thread| thread.latest.id == visible.id)
            .count(),
        1
    );
}

#[tokio::test]
async fn latest_thread_category_selects_one_section_without_duplicates() {
    let store = Store::in_memory().await.unwrap();
    let account_id = uuid::Uuid::new_v4();

    let mut older_pending = message(account_id, "INBOX", 10, "classified-latest", 0);
    older_pending.message_id = Some("<classified-root@example.test>".into());
    let mut newer_classified = message(account_id, "INBOX", 11, "classified-latest", 1);
    newer_classified.category = Some("people".into());
    newer_classified.in_reply_to = older_pending.message_id.clone();
    newer_classified.reference_ids = older_pending.message_id.clone();

    let mut older_classified = message(account_id, "INBOX", 12, "pending-latest", 2);
    older_classified.category = Some("transactions".into());
    older_classified.message_id = Some("<pending-root@example.test>".into());
    let mut newer_pending = message(account_id, "INBOX", 13, "pending-latest", 3);
    newer_pending.in_reply_to = older_classified.message_id.clone();
    newer_pending.reference_ids = older_classified.message_id.clone();

    let mut flagged_pending = message(account_id, "INBOX", 14, "flagged-pending", 4);
    flagged_pending.is_flagged = true;
    store
        .upsert_messages(&[
            older_pending,
            newer_classified.clone(),
            older_classified,
            newer_pending.clone(),
            flagged_pending.clone(),
        ])
        .await
        .unwrap();

    let page = store
        .search_smart_inbox(&SmartInboxQuery {
            account_ids: vec![account_id],
            limit: Some(10),
        })
        .await
        .unwrap();

    assert_eq!(
        section(&page, "people").conversations[0].latest.id,
        newer_classified.id
    );
    assert_eq!(
        section(&page, "other").conversations[0].latest.id,
        newer_pending.id
    );
    assert_eq!(
        section(&page, "starred").conversations[0].latest.id,
        flagged_pending.id
    );
    for expected in [newer_classified.id, newer_pending.id, flagged_pending.id] {
        assert_eq!(
            page.sections
                .iter()
                .flat_map(|section| &section.conversations)
                .filter(|thread| thread.latest.id == expected)
                .count(),
            1,
            "a conversation must belong to exactly one initial Smart section"
        );
    }
    assert!(section(&page, "transactions").conversations.is_empty());
}

#[tokio::test]
async fn external_flag_change_moves_a_read_conversation_from_seen_to_starred_once() {
    let store = Store::in_memory().await.unwrap();
    let account_id = uuid::Uuid::new_v4();
    let mut external = message(account_id, "INBOX", 4, "external-star", 0);
    external.is_read = true;
    store.upsert_messages(&[external.clone()]).await.unwrap();

    let before = store
        .search_smart_inbox(&SmartInboxQuery {
            account_ids: vec![account_id],
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(
        section(&before, "seen").conversations[0].latest.id,
        external.id
    );
    assert!(section(&before, "starred").conversations.is_empty());

    // This models a second client changing only the IMAP flag. The existing
    // local message remains read, so it must leave Seen when Starred takes
    // priority.
    store
        .update_mailbox_flags(account_id, "INBOX", &[(4, true, true)])
        .await
        .unwrap();

    let after = store
        .search_smart_inbox(&SmartInboxQuery {
            account_ids: vec![account_id],
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(
        section(&after, "starred").conversations[0].latest.id,
        external.id
    );
    assert!(section(&after, "seen").conversations.is_empty());
    assert_eq!(
        after
            .sections
            .iter()
            .flat_map(|section| &section.conversations)
            .filter(|thread| thread.latest.id == external.id)
            .count(),
        1,
        "an externally flagged read conversation must have one Smart section"
    );
}
