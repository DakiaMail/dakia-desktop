use chrono::{TimeZone, Utc};
use dakia_core::{
    provider, Account, AccountDraft, AccountId, MailSummary, ModelClassificationUpdate,
    SearchQuery, Store,
};

fn account() -> Account {
    let mut account = AccountDraft {
        email: "epoch@example.test".into(),
        display_name: "Epoch test".into(),
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
    account.id = AccountId::from_u128(0xe0c0);
    account
}

fn message(account: AccountId, subject: &str) -> MailSummary {
    MailSummary {
        // This is intentionally stable across the simulated UIDVALIDITY
        // replacement. The epoch is what prevents a stale visible row from
        // targeting the new message that reused this locator.
        id: format!("{account}-INBOX-7"),
        account_id: account.to_string(),
        mailbox: "INBOX".into(),
        uid: 7,
        mailbox_uid_validity: None,
        message_id: Some("<same-locator@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "epoch-thread".into(),
        subject: subject.into(),
        from_name: Some("Epoch sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "epoch@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc
            .with_ymd_and_hms(2026, 10, 17, 12, 0, 0)
            .single()
            .expect("fixed time"),
        snippet: subject.into(),
        body_text: subject.into(),
        body_html: None,
        content_state: "complete".into(),
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

#[tokio::test]
async fn user_visible_message_reads_carry_the_catalogued_uidvalidity_epoch() {
    let store = Store::in_memory().await.expect("open store");
    let account = account();
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 101, 1, true)
        .await
        .expect("save initial epoch");
    let original = message(account.id, "old UID namespace");
    assert!(
        serde_json::to_value(&original)
            .expect("serialize unpersisted summary")
            .get("mailbox_uid_validity")
            .is_none(),
        "parsed or unprojected summaries must not invent an epoch"
    );
    store
        .upsert_messages(std::slice::from_ref(&original))
        .await
        .expect("save initial message");

    let retained_old_snapshot = store
        .message(&original.id)
        .await
        .expect("read old message")
        .expect("old message exists");
    assert_eq!(retained_old_snapshot.mailbox_uid_validity, Some(101));
    let old_json = serde_json::to_value(&retained_old_snapshot).expect("serialize summary");
    assert_eq!(old_json["mailbox_uid_validity"], 101);
    assert!(old_json.get("mailboxUidValidity").is_none());

    let query = SearchQuery {
        account_ids: vec![account.id],
        mailbox: Some("INBOX".into()),
        ..SearchQuery::default()
    };
    assert_eq!(
        store.search(&query).await.expect("search old mailbox")[0].mailbox_uid_validity,
        Some(101)
    );
    assert_eq!(
        store
            .search_conversation_page(&query)
            .await
            .expect("page old mailbox")
            .conversations[0]
            .latest
            .mailbox_uid_validity,
        Some(101)
    );

    // A UIDVALIDITY rollover clears the old namespace and permits the server
    // to reuse UID 7. The local stable ID is intentionally unchanged.
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 202, 1, true)
        .await
        .expect("save replacement epoch");
    let replacement = message(account.id, "new UID namespace");
    assert_eq!(replacement.id, original.id);
    store
        .replace_mailbox_messages(account.id, "INBOX", &[replacement])
        .await
        .expect("replace mailbox messages");

    let current = store
        .message(&original.id)
        .await
        .expect("read replacement")
        .expect("replacement exists");
    assert_eq!(current.subject, "new UID namespace");
    assert_eq!(current.mailbox_uid_validity, Some(202));
    assert_eq!(retained_old_snapshot.mailbox_uid_validity, Some(101));

    assert_eq!(
        store.search(&query).await.expect("search replacement")[0].mailbox_uid_validity,
        Some(202)
    );
    let page = store
        .search_conversation_page(&query)
        .await
        .expect("page replacement");
    assert_eq!(page.conversations[0].latest.mailbox_uid_validity, Some(202));
    assert_eq!(
        page.conversations[0].source_messages[0].mailbox_uid_validity,
        Some(202)
    );
}

#[tokio::test]
async fn maintenance_candidates_carry_epochs_and_stale_classifier_receipts_are_suppressed() {
    let store = Store::in_memory().await.expect("open store");
    let account = account();
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 2, true)
        .await
        .expect("save initial epoch");

    let mut hydration = message(account.id, "candidate needing hydration");
    hydration.id = "hydration-candidate".into();
    hydration.content_state = "headers_only".into();
    hydration.is_flagged = true;
    hydration.uid = 6;

    let mut classified = message(account.id, "same evidence in old epoch");
    classified.id = "classification-candidate".into();
    classified.uid = 7;
    store
        .upsert_messages(&[hydration, classified])
        .await
        .expect("save maintenance candidates");

    let cutoff = Utc
        .with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
        .single()
        .expect("fixed cutoff");
    assert_eq!(
        store
            .recent_body_cache_candidates_page(account.id, cutoff, 10, 0)
            .await
            .expect("read recent cache candidates")
            .into_iter()
            .find(|message| message.id == "hydration-candidate")
            .expect("hydration candidate")
            .mailbox_uid_validity,
        Some(77)
    );
    assert_eq!(
        store
            .incomplete_inbox_messages(account.id, 10)
            .await
            .expect("read incomplete candidates")[0]
            .mailbox_uid_validity,
        Some(77)
    );
    assert_eq!(
        store
            .uncached_starred_messages(account.id, 10)
            .await
            .expect("read starred candidates")[0]
            .mailbox_uid_validity,
        Some(77)
    );

    store
        .claim_classification_revision("epoch-owner", "epoch-model")
        .await
        .expect("claim classifier revision");
    let old_receipt_source = store
        .messages_for_model_classification()
        .await
        .expect("read classifier candidates")
        .into_iter()
        .find(|message| message.id == "classification-candidate")
        .expect("classification candidate");
    assert_eq!(old_receipt_source.mailbox_uid_validity, Some(77));
    let stale_receipt = ModelClassificationUpdate::from_message(
        &old_receipt_source,
        "people".into(),
        Some(0.9),
        false,
        "epoch-owner",
        "epoch-model",
    );

    // This is a complete UIDVALIDITY replacement with the same stable ID,
    // UID, and classification evidence. Only the captured epoch distinguishes
    // the old receipt from the new message.
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 78, 1, true)
        .await
        .expect("save replacement epoch");
    let mut replacement = message(account.id, "same evidence in old epoch");
    replacement.id = "classification-candidate".into();
    replacement.uid = 7;
    assert_eq!(replacement.id, old_receipt_source.id);
    assert_eq!(replacement.uid, old_receipt_source.uid);
    store
        .replace_mailbox_messages(account.id, "INBOX", &[replacement])
        .await
        .expect("replace mailbox namespace");

    assert_eq!(
        store
            .apply_model_classifications(&[stale_receipt])
            .await
            .expect("apply stale classifier receipt"),
        0
    );
    let current = store
        .message("classification-candidate")
        .await
        .expect("read replacement")
        .expect("replacement exists");
    assert_eq!(current.mailbox_uid_validity, Some(78));
    assert_eq!(current.category, None);
    assert_eq!(current.classification_source, None);
}

#[tokio::test]
async fn rebuilding_threads_recomputes_a_reused_stable_id_in_the_new_epoch() {
    let store = Store::in_memory().await.expect("open store");
    let account = account();
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 2, true)
        .await
        .expect("save old epoch");
    let mut root = message(account.id, "root");
    root.id = "thread-root".into();
    root.uid = 1;
    root.message_id = Some("<root@example.test>".into());
    let mut old = message(account.id, "old reply");
    old.id = "reused-stable-id".into();
    old.uid = 7;
    old.message_id = Some("<old@example.test>".into());
    old.in_reply_to = Some("<root@example.test>".into());
    store
        .upsert_messages(&[root, old])
        .await
        .expect("save old graph");
    assert_eq!(
        store
            .message("reused-stable-id")
            .await
            .unwrap()
            .unwrap()
            .thread_id,
        "root@example.test"
    );

    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 78, 2, true)
        .await
        .expect("save new epoch");
    let mut replacement = message(account.id, "replacement");
    replacement.id = "reused-stable-id".into();
    replacement.uid = 7;
    replacement.message_id = Some("<replacement@example.test>".into());
    replacement.in_reply_to = None;
    replacement.reference_ids = None;
    store
        .upsert_messages(&[replacement])
        .await
        .expect("replace reused stable id");
    let current = store.message("reused-stable-id").await.unwrap().unwrap();
    assert_eq!(current.mailbox_uid_validity, Some(78));
    assert_eq!(current.thread_id, "replacement@example.test");
}
