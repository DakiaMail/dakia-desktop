use super::*;
use crate::{account::AccountDraft, provider};

fn test_account(id: AccountId) -> Account {
    let mut account = AccountDraft {
        email: format!("{id}@example.test"),
        display_name: "Realtime publication test".into(),
        provider_id: Some("fastmail".into()),
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
    .into_account(provider::by_id("fastmail").unwrap());
    account.id = id;
    account
}

fn message(account_id: AccountId, uid: u32, is_read: bool, is_flagged: bool) -> MailSummary {
    MailSummary {
        id: stable_message_id(account_id, "INBOX", uid),
        account_id: account_id.to_string(),
        mailbox: "INBOX".into(),
        uid: i64::from(uid),
        mailbox_uid_validity: Some(41),
        message_id: Some(format!("<realtime-{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("thread-{uid}"),
        subject: format!("Fictional message {uid}"),
        from_name: Some("Fictional Sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: format!("preview {uid}"),
        body_text: String::new(),
        body_html: None,
        content_state: "headers_only".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read,
        is_flagged,
        has_attachments: false,
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: Vec::new(),
    }
}

async fn store_with_messages(messages: &[(u32, bool, bool)]) -> (Store, AccountId) {
    let store = Store::in_memory().await.unwrap();
    let account_id = uuid::Uuid::new_v4();
    store.save_account(&test_account(account_id)).await.unwrap();
    let rows = messages
        .iter()
        .map(|(uid, read, flagged)| message(account_id, *uid, *read, *flagged))
        .collect::<Vec<_>>();
    store
        .save_synced_messages(account_id, "INBOX", &rows)
        .await
        .unwrap();
    store
        .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 41, rows.len(), true)
        .await
        .unwrap();
    store
        .set_mailbox_uid_validity(account_id, "INBOX", Some(41))
        .await
        .unwrap();
    (store, account_id)
}

async fn begin_snapshot(
    store: &Store,
    account_id: AccountId,
    uid_validity: u32,
    total: u32,
) -> String {
    store
        .begin_mailbox_snapshot(
            account_id,
            "INBOX",
            MailboxSnapshotIdentity::new("INBOX", uid_validity, total, Some(total + 1), None),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn realtime_publication_distinguishes_noop_from_real_flag_changes() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;

    let generation = begin_snapshot(&store, account_id, 41, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(1, false, false)])
        .await
        .unwrap();
    let no_op = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &generation,
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(no_op, MailboxSnapshotPublication::default());
    assert!(!no_op.catalogue_changed());

    let generation = begin_snapshot(&store, account_id, 41, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(1, true, true)])
        .await
        .unwrap();
    let changed = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &generation,
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(changed.flags_changed, 1);
    assert_eq!(changed.metadata_published, 0);
    assert_eq!(changed.deleted, 0);
    assert!(!changed.namespace_replaced);
    assert!(changed.catalogue_changed());
    let persisted = store
        .message(&stable_message_id(account_id, "INBOX", 1))
        .await
        .unwrap()
        .unwrap();
    assert!(persisted.is_read);
    assert!(persisted.is_flagged);
}

#[tokio::test]
async fn deleted_only_finalization_skips_exact_stats_while_realtime_executes_them() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;

    let generation = begin_snapshot(&store, account_id, 41, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(1, true, false)])
        .await
        .unwrap();
    let public_queries = SNAPSHOT_PUBLICATION_STAT_QUERIES
        .scope(std::cell::Cell::new(0), async {
            let deleted = store
                .finalize_mailbox_snapshot_with_staged_messages(account_id, "INBOX", &generation)
                .await
                .unwrap();
            assert_eq!(deleted, 0);
            SNAPSHOT_PUBLICATION_STAT_QUERIES.with(std::cell::Cell::get)
        })
        .await;
    assert_eq!(
        public_queries, 0,
        "deleted-only callers must not execute exact publication statistics"
    );

    let generation = begin_snapshot(&store, account_id, 41, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(1, false, false)])
        .await
        .unwrap();
    let (publication, realtime_queries) = SNAPSHOT_PUBLICATION_STAT_QUERIES
        .scope(std::cell::Cell::new(0), async {
            let publication = store
                .finalize_realtime_mailbox_snapshot_with_staged_messages(
                    account_id,
                    "INBOX",
                    &generation,
                    Some(1),
                )
                .await
                .unwrap();
            let queries = SNAPSHOT_PUBLICATION_STAT_QUERIES.with(std::cell::Cell::get);
            (publication, queries)
        })
        .await;
    assert_eq!(publication.flags_changed, 1);
    assert_eq!(publication.metadata_published, 0);
    assert_eq!(
        realtime_queries, 2,
        "realtime finalization must execute flag and staged-metadata statistics"
    );
}

#[tokio::test]
async fn realtime_publication_reports_insert_and_delete_from_the_same_generation() {
    let (store, account_id) = store_with_messages(&[(1, false, false), (3, false, false)]).await;
    let generation = begin_snapshot(&store, account_id, 41, 2).await;
    store
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &generation,
            &[(1, false, false), (2, false, false)],
        )
        .await
        .unwrap();
    store
        .stage_mailbox_snapshot_message_page(
            account_id,
            "INBOX",
            &generation,
            &[message(account_id, 2, false, false)],
        )
        .await
        .unwrap();

    let publication = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &generation,
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(publication.metadata_published, 1);
    assert_eq!(publication.deleted, 1);
    assert_eq!(publication.flags_changed, 0);
    assert!(publication.catalogue_changed());
    assert!(store
        .message(&stable_message_id(account_id, "INBOX", 2))
        .await
        .unwrap()
        .is_some());
    assert!(store
        .message(&stable_message_id(account_id, "INBOX", 3))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn realtime_publication_does_not_report_tombstone_suppressed_metadata() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let generation = begin_snapshot(&store, account_id, 41, 2).await;
    store
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &generation,
            &[(1, false, false), (2, false, false)],
        )
        .await
        .unwrap();
    store
        .stage_mailbox_snapshot_message_page(
            account_id,
            "INBOX",
            &generation,
            &[message(account_id, 2, false, false)],
        )
        .await
        .unwrap();
    sqlx::query("INSERT INTO mailbox_action_tombstones(account_id, mailbox, uid, created_at) VALUES (?, 'INBOX', 2, ?)")
        .bind(account_id.to_string())
        .bind(Utc::now())
        .execute(&store.pool)
        .await
        .unwrap();

    let publication = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &generation,
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(publication, MailboxSnapshotPublication::default());
    assert!(!publication.catalogue_changed());
    assert!(store
        .message(&stable_message_id(account_id, "INBOX", 2))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn empty_uidvalidity_rollover_is_a_catalogue_change() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let generation = begin_snapshot(&store, account_id, 42, 0).await;

    let publication = store
        .finalize_realtime_mailbox_snapshot_with_staged_replacements(
            account_id,
            "INBOX",
            &generation,
            None,
        )
        .await
        .unwrap();
    assert_eq!(publication.metadata_published, 0);
    assert_eq!(publication.flags_changed, 0);
    assert_eq!(publication.deleted, 0);
    assert!(publication.namespace_replaced);
    assert!(publication.catalogue_changed());
    assert!(store
        .message(&stable_message_id(account_id, "INBOX", 1))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn nonempty_uidvalidity_rollover_reports_metadata_and_namespace_change() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let generation = begin_snapshot(&store, account_id, 42, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &generation, &[(2, false, false)])
        .await
        .unwrap();
    store
        .stage_mailbox_snapshot_message_page(
            account_id,
            "INBOX",
            &generation,
            &[message(account_id, 2, false, false)],
        )
        .await
        .unwrap();

    let publication = store
        .finalize_realtime_mailbox_snapshot_with_staged_replacements(
            account_id,
            "INBOX",
            &generation,
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(publication.metadata_published, 1);
    assert!(publication.namespace_replaced);
    assert!(publication.catalogue_changed());
    assert!(store
        .message(&stable_message_id(account_id, "INBOX", 1))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .message(&stable_message_id(account_id, "INBOX", 2))
            .await
            .unwrap()
            .unwrap()
            .mailbox_uid_validity,
        Some(42)
    );
}

#[tokio::test]
async fn stale_generation_cannot_publish_through_realtime_outcome() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let stale = begin_snapshot(&store, account_id, 41, 1).await;
    store
        .stage_mailbox_snapshot_page(account_id, "INBOX", &stale, &[(1, true, true)])
        .await
        .unwrap();
    let replacement = begin_snapshot(&store, account_id, 41, 2).await;

    let error = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &stale,
            Some(1),
        )
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("snapshot generation is not active"));
    let persisted = store
        .message(&stable_message_id(account_id, "INBOX", 1))
        .await
        .unwrap()
        .unwrap();
    assert!(!persisted.is_read);
    assert!(!persisted.is_flagged);
    assert_eq!(
        store
            .staged_mailbox_snapshot_uids(account_id, "INBOX", &replacement)
            .await
            .unwrap(),
        Vec::<u32>::new()
    );
}

#[tokio::test]
async fn same_identity_resume_from_a_second_store_cannot_publish_an_emptied_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("same-identity-snapshot-race.db");
    let first = Store::open(&database).await.unwrap();
    let second = Store::open(&database).await.unwrap();
    let account_id = uuid::Uuid::new_v4();
    first.save_account(&test_account(account_id)).await.unwrap();
    let original = [
        message(account_id, 1, false, false),
        message(account_id, 2, false, false),
    ];
    first
        .save_synced_messages(account_id, "INBOX", &original)
        .await
        .unwrap();
    first
        .save_mailbox_catalog_state(account_id, "INBOX", "INBOX", 41, 2, true)
        .await
        .unwrap();
    first
        .set_mailbox_uid_validity(account_id, "INBOX", Some(41))
        .await
        .unwrap();

    let staged = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let (resume_complete, resume_finished) = tokio::sync::oneshot::channel();
    let first_store = first.clone();
    let first_staged = staged.clone();
    let first_finalizer = tokio::spawn(async move {
        let generation = begin_snapshot(&first_store, account_id, 41, 2).await;
        first_store
            .stage_mailbox_snapshot_page(
                account_id,
                "INBOX",
                &generation,
                &[(1, true, false), (2, false, true)],
            )
            .await
            .unwrap();
        first_staged.wait().await;
        resume_finished.await.unwrap();
        first_store
            .finalize_realtime_mailbox_snapshot_with_staged_messages(
                account_id,
                "INBOX",
                &generation,
                Some(2),
            )
            .await
            .unwrap_err()
    });

    let second_store = second.clone();
    let second_staged = staged.clone();
    let resumer = tokio::spawn(async move {
        second_staged.wait().await;
        let resumed_generation = begin_snapshot(&second_store, account_id, 41, 2).await;
        resume_complete.send(()).unwrap();
        resumed_generation
    });

    let error = first_finalizer.await.unwrap();
    let resumed_generation = resumer.await.unwrap();
    assert!(
        error
            .to_string()
            .contains("snapshot inventory is incomplete"),
        "{error:#}"
    );
    assert_eq!(
        first.mailbox_uids(account_id, "INBOX").await.unwrap(),
        [1, 2].into_iter().collect(),
        "a reset staging generation must not delete the committed mailbox"
    );
    let catalog = first
        .mailbox_catalog_state(account_id, "INBOX")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(catalog.remote_total, 2);
    assert_eq!(catalog.uid_next, None);
    assert_eq!(
        first
            .highest_mailbox_uid(account_id, "INBOX")
            .await
            .unwrap(),
        Some(2)
    );

    second
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &resumed_generation,
            &[(1, true, false)],
        )
        .await
        .unwrap();
    let partial_error = second
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &resumed_generation,
            Some(2),
        )
        .await
        .unwrap_err();
    assert!(
        partial_error
            .to_string()
            .contains("snapshot inventory is incomplete"),
        "{partial_error:#}"
    );
    assert_eq!(
        first.mailbox_uids(account_id, "INBOX").await.unwrap(),
        [1, 2].into_iter().collect(),
        "a partial restage must leave the committed mailbox intact"
    );
    let catalog = first
        .mailbox_catalog_state(account_id, "INBOX")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(catalog.remote_total, 2);
    assert_eq!(catalog.uid_next, None);
    assert_eq!(
        first
            .highest_mailbox_uid(account_id, "INBOX")
            .await
            .unwrap(),
        Some(2)
    );

    second
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &resumed_generation,
            &[(2, false, true)],
        )
        .await
        .unwrap();
    let recovered = second
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &resumed_generation,
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(recovered.flags_changed, 2);
    assert_eq!(recovered.deleted, 0);
    assert_eq!(
        first.mailbox_uids(account_id, "INBOX").await.unwrap(),
        [1, 2].into_iter().collect(),
        "a completed restage must retain the recovered namespace"
    );

    let converged_generation = begin_snapshot(&second, account_id, 41, 2).await;
    second
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &converged_generation,
            &[(1, true, false), (2, false, true)],
        )
        .await
        .unwrap();
    let converged = second
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &converged_generation,
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(converged, MailboxSnapshotPublication::default());
}

#[tokio::test]
async fn removed_account_cannot_publish_staged_realtime_metadata() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let generation = begin_snapshot(&store, account_id, 41, 2).await;
    store
        .stage_mailbox_snapshot_page(
            account_id,
            "INBOX",
            &generation,
            &[(1, false, false), (2, false, false)],
        )
        .await
        .unwrap();
    store
        .stage_mailbox_snapshot_message_page(
            account_id,
            "INBOX",
            &generation,
            &[message(account_id, 2, false, false)],
        )
        .await
        .unwrap();
    store.delete_account(account_id).await.unwrap();

    let error = store
        .finalize_realtime_mailbox_snapshot_with_staged_messages(
            account_id,
            "INBOX",
            &generation,
            Some(2),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("account was removed"));
    let message_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE account_id = ?")
            .bind(account_id.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(message_count, 0);
}

#[tokio::test]
async fn changed_since_publication_counts_only_visible_flag_deltas() {
    let (store, account_id) = store_with_messages(&[(1, false, false)]).await;
    let identity = MailboxSnapshotIdentity::new("INBOX", 41, 1, Some(2), Some(8));
    let no_op = store
        .apply_realtime_mailbox_changed_since_flags(
            account_id,
            "INBOX",
            MailboxChangedSinceFlags {
                identity,
                remote_total: 1,
                flags: &[(1, false, false)],
            },
        )
        .await
        .unwrap();
    assert_eq!(no_op, 0);

    let changed = store
        .apply_realtime_mailbox_changed_since_flags(
            account_id,
            "INBOX",
            MailboxChangedSinceFlags {
                identity,
                remote_total: 1,
                flags: &[(1, true, false)],
            },
        )
        .await
        .unwrap();
    assert_eq!(changed, 1);
}
