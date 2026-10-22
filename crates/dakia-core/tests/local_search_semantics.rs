use chrono::{DateTime, Utc};
use dakia_core::{
    provider, AccountDraft, MailConversationPage, MailCursor, MailSummary, SearchQuery, Store,
};
use std::collections::HashSet;
use tempfile::tempdir;
use uuid::Uuid;

fn message(
    id: &str,
    account_id: Uuid,
    mailbox: &str,
    uid: i64,
    subject: &str,
    body: &str,
    received_at: DateTime<Utc>,
) -> MailSummary {
    MailSummary {
        id: id.into(),
        account_id: account_id.to_string(),
        mailbox: mailbox.into(),
        uid,
        mailbox_uid_validity: Some(77),
        message_id: Some(format!("<{id}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: id.into(),
        subject: subject.into(),
        from_name: Some("Fictional Search Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at,
        snippet: body.into(),
        body_text: body.into(),
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
async fn conversation_search_escapes_unicode_and_keeps_colliding_accounts_scoped() {
    let store = Store::in_memory().await.expect("open store");
    let selected = Uuid::new_v4();
    let other = Uuid::new_v4();
    let timestamp = "2026-10-21T08:00:00Z".parse().unwrap();
    let mut selected_match = message(
        "selected-inbox",
        selected,
        "INBOX",
        42,
        "雪 töö tag",
        "Fictional local match",
        timestamp,
    );
    let mut other_collision = message(
        "other-inbox",
        other,
        "INBOX",
        42,
        "雪 töö tag",
        "Same UID and logical identity in another account",
        timestamp,
    );
    selected_match.thread_id = "colliding-thread".into();
    other_collision.thread_id = "colliding-thread".into();
    selected_match.message_id = Some("<colliding@example.test>".into());
    other_collision.message_id = Some("<colliding@example.test>".into());
    store
        .upsert_messages(&[
            selected_match,
            other_collision,
            message(
                "selected-trash",
                selected,
                "Trash::Deleted",
                43,
                "雪 töö tag",
                "Wrong mailbox",
                timestamp,
            ),
            message(
                "selected-nonmatch",
                selected,
                "INBOX",
                44,
                "Ordinary subject",
                "No matching tokens",
                timestamp,
            ),
        ])
        .await
        .expect("index fictional messages");

    let page = store
        .search_conversation_page(&SearchQuery {
            text: "雪 \"töö\" +tag".into(),
            account_ids: vec![selected],
            mailbox: Some("INBOX".into()),
            ..SearchQuery::default()
        })
        .await
        .expect("search conversation catalogue");

    assert_eq!(page.conversations.len(), 1);
    assert_eq!(page.conversations[0].account_id, selected.to_string());
    assert_eq!(page.conversations[0].latest.id, "selected-inbox");
    assert_eq!(page.conversations[0].thread_id, "colliding@example.test");
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn text_filtered_conversation_pages_are_complete_stable_and_non_overlapping() {
    let store = Store::in_memory().await.expect("open store");
    let selected = Uuid::new_v4();
    let other = Uuid::new_v4();
    let timestamp = "2026-10-21T08:00:00Z".parse().unwrap();
    let mut messages = (0..5)
        .map(|index| {
            message(
                &format!("selected-{index}"),
                selected,
                "INBOX",
                index + 1,
                &format!("東京 result {index}"),
                "Paged Unicode match",
                timestamp,
            )
        })
        .collect::<Vec<_>>();
    let root_message_id = messages[0].message_id.clone().expect("root message id");
    let mut first_reply = message(
        "selected-shared-reply-1",
        selected,
        "INBOX",
        20,
        "Re: 東京 result 0",
        "Paged Unicode match",
        timestamp - chrono::Duration::seconds(1),
    );
    first_reply.in_reply_to = Some(root_message_id.clone());
    first_reply.reference_ids = Some(root_message_id.clone());
    let mut second_reply = message(
        "selected-shared-reply-2",
        selected,
        "INBOX",
        21,
        "Re: 東京 result 0",
        "Paged Unicode match",
        timestamp - chrono::Duration::seconds(2),
    );
    second_reply.in_reply_to = Some(root_message_id.clone());
    second_reply.reference_ids = Some(root_message_id);
    messages.extend([first_reply, second_reply]);
    messages.push(message(
        "other-account-match",
        other,
        "INBOX",
        1,
        "東京 result other",
        "Must remain outside selected scope",
        timestamp,
    ));
    store
        .upsert_messages(&messages)
        .await
        .expect("index paged messages");

    let base = SearchQuery {
        text: "東京".into(),
        account_ids: vec![selected],
        mailbox: Some("INBOX".into()),
        limit: Some(2),
        ..SearchQuery::default()
    };
    let mut cursor: Option<MailCursor> = None;
    let mut seen = Vec::new();
    let mut shared_thread_pages = 0;
    loop {
        let page = store
            .search_conversation_page(&SearchQuery {
                cursor: cursor.clone(),
                ..base.clone()
            })
            .await
            .expect("load local search page");
        seen.extend(
            page.conversations
                .iter()
                .map(|conversation| conversation.latest.id.clone()),
        );
        for conversation in &page.conversations {
            if conversation.message_count == 3 {
                shared_thread_pages += 1;
                assert_eq!(conversation.messages.len(), 3);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert_eq!(seen.len(), 5);
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 5);
    assert!(seen.iter().all(|id| id.starts_with("selected-")));
    assert_eq!(shared_thread_pages, 1);
}

#[test]
fn local_search_contract_round_trips_rust_query_and_nonempty_page() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../apps/desktop/testdata/tauri-contracts/local-search.json"
    ))
    .expect("parse shared local search contract");
    let query: SearchQuery = serde_json::from_value(fixture["arguments"]["query"].clone())
        .expect("deserialize SearchQuery");
    assert_eq!(query.text, "  雪 \"töö\" +tag  ");
    assert_eq!(query.account_ids.len(), 1);
    assert_eq!(query.limit, Some(500));
    assert_eq!(
        query.cursor.as_ref().map(|cursor| cursor.id.as_str()),
        Some("account-a:INBOX:雪-042")
    );
    assert_eq!(
        serde_json::to_value(&query).expect("serialize SearchQuery"),
        fixture["arguments"]["query"]
    );

    let page: MailConversationPage = serde_json::from_value(fixture["response"].clone())
        .expect("deserialize MailConversationPage");
    assert_eq!(page.conversations.len(), 1);
    assert_eq!(page.conversations[0].latest.id, "unicode-message");
    assert_eq!(
        page.conversations[0].account_id,
        query.account_ids[0].to_string()
    );
    assert_eq!(
        serde_json::to_value(page).expect("serialize MailConversationPage"),
        fixture["response"]
    );
}

#[tokio::test]
async fn updated_fts_content_survives_reopen_without_retaining_the_old_term() {
    let directory = tempdir().expect("temporary profile");
    let database = directory.path().join("dakia.db");
    let timestamp = "2026-10-21T08:00:00Z".parse().unwrap();
    let store = Store::open(&database).await.expect("open profile");
    let account = AccountDraft {
        email: "local-search@example.test".into(),
        display_name: "Local search fixture".into(),
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
    let account_id = account.id;
    store.save_account(&account).await.expect("save account");
    let mut indexed = message(
        "updated-message",
        account_id,
        "INBOX",
        1,
        "Old Reykjavík itinerary",
        "obsolete search token",
        timestamp,
    );
    store
        .upsert_messages(&[indexed.clone()])
        .await
        .expect("index original message");
    indexed.subject = "New München itinerary".into();
    indexed.body_text = "replacement search token".into();
    indexed.snippet = indexed.body_text.clone();
    store
        .upsert_messages(&[indexed])
        .await
        .expect("update indexed message");
    let before_reopen = store
        .search_conversation_page(&SearchQuery {
            text: "München replacement".into(),
            account_ids: vec![account_id],
            mailbox: Some("INBOX".into()),
            ..SearchQuery::default()
        })
        .await
        .expect("search updated terms before reopen");
    assert_eq!(before_reopen.conversations.len(), 1);
    drop(store);

    let reopened = Store::open(&database).await.expect("reopen profile");
    let search = |text: &str| SearchQuery {
        text: text.into(),
        account_ids: vec![account_id],
        mailbox: Some("INBOX".into()),
        ..SearchQuery::default()
    };
    let all = reopened
        .search_conversation_page(&search(""))
        .await
        .expect("read reopened catalogue");
    assert_eq!(all.conversations[0].latest.subject, "New München itinerary");
    assert!(reopened
        .search_conversation_page(&search("obsolete"))
        .await
        .expect("search old term")
        .conversations
        .is_empty());
    let replacement = reopened
        .search_conversation_page(&search("München replacement"))
        .await
        .expect("search replacement terms");
    assert_eq!(replacement.conversations.len(), 1);
    assert_eq!(replacement.conversations[0].latest.id, "updated-message");
}
