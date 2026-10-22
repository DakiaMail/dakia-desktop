use chrono::{Duration, TimeZone, Utc};
use dakia_core::{
    provider, Account, AccountDraft, AccountId, MailSummary, MailboxMoveDestination,
    MailboxMoveReceipt, ModelClassificationUpdate, SearchQuery, Store,
};
use std::collections::HashSet;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

fn account(id: u128, label: &str) -> Account {
    let mut account = AccountDraft {
        email: format!("{label}@example.test"),
        display_name: format!("{label} account"),
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
    account.id = AccountId::from_u128(id);
    account
}

fn message(account_id: AccountId, mailbox: &str, uid: u32, label: &str) -> MailSummary {
    MailSummary {
        id: format!("{account_id}:{mailbox}:{uid}"),
        account_id: account_id.to_string(),
        mailbox: mailbox.into(),
        uid: i64::from(uid),
        mailbox_uid_validity: None,
        message_id: Some(format!("<{label}-{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("thread-{account_id}-{uid}"),
        subject: format!("{label} subject {uid}"),
        from_name: Some("Fictional sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: format!("{label}@example.test"),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc
            .with_ymd_and_hms(2026, 10, 17, 12, 0, 0)
            .single()
            .expect("fixed timestamp")
            + Duration::seconds(i64::from(uid)),
        snippet: format!("{label} body {uid}"),
        body_text: format!("{label} body {uid}"),
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

async fn seed_mailbox(store: &Store, account: &Account, mailbox: &str, epoch: u32) {
    store.save_account(account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, mailbox, mailbox, epoch, 1, true)
        .await
        .expect("save mailbox epoch");
}

#[tokio::test]
async fn account_removal_fences_every_delayed_storage_work_class() {
    let temporary = TempDir::new().expect("temporary directory");
    let database = temporary.path().join("account-removal.sqlite");
    let store = Store::open(&database).await.expect("open store");
    let deleted = account(0x0d31_e7ed, "deleted");
    let survivor = account(0x0051_a71e, "survivor");
    seed_mailbox(&store, &deleted, "INBOX", 77).await;
    seed_mailbox(&store, &survivor, "INBOX", 88).await;

    let original = message(deleted.id, "INBOX", 7, "deleted");
    let surviving_message = message(survivor.id, "INBOX", 7, "survivor");
    store
        .upsert_messages(&[original.clone(), surviving_message.clone()])
        .await
        .expect("seed messages");
    let target = store
        .message(&original.id)
        .await
        .expect("read target")
        .expect("target exists");
    let mut fetched = target.clone();
    fetched.body_text = "late fetched body".into();
    fetched.has_attachments = true;

    store
        .claim_classification_revision("removal-owner", "removal-model")
        .await
        .expect("claim classifier revision");
    let classification = ModelClassificationUpdate::from_message(
        &target,
        "people".into(),
        Some(0.9),
        false,
        "removal-owner",
        "removal-model",
    );

    // Each task captures its immutable input before deletion, reports that it
    // is ready, and then waits on the same release signal. This models work
    // that finished its provider or CPU phase before account removal without
    // relying on scheduler timing or sleeps.
    let (ready_tx, mut ready_rx) = mpsc::channel::<&'static str>(6);
    let (release_tx, release_rx) = watch::channel(false);

    let sync = {
        let store = store.clone();
        let mut release = release_rx.clone();
        let ready = ready_tx.clone();
        let late = message(deleted.id, "INBOX", 8, "late-sync");
        tokio::spawn(async move {
            ready.send("sync").await.expect("announce sync");
            release.changed().await.expect("release sync");
            store
                .save_synced_messages_through_at_epoch(deleted.id, "INBOX", &[late], Some(8), 77)
                .await
        })
    };
    let content = {
        let store = store.clone();
        let target = target.clone();
        let fetched = fetched.clone();
        let mut release = release_rx.clone();
        let ready = ready_tx.clone();
        tokio::spawn(async move {
            ready.send("content").await.expect("announce content");
            release.changed().await.expect("release content");
            store
                .publish_message_content_at_epoch(&target, &fetched)
                .await
        })
    };
    let attachment = {
        let store = store.clone();
        let id = target.id.clone();
        let mut release = release_rx.clone();
        let ready = ready_tx.clone();
        tokio::spawn(async move {
            ready.send("attachment").await.expect("announce attachment");
            release.changed().await.expect("release attachment");
            store.update_message_attachment_state(&id, true).await
        })
    };
    let classify = {
        let store = store.clone();
        let mut release = release_rx.clone();
        let ready = ready_tx.clone();
        tokio::spawn(async move {
            ready
                .send("classification")
                .await
                .expect("announce classification");
            release.changed().await.expect("release classification");
            store.apply_model_classifications(&[classification]).await
        })
    };
    let read = {
        let store = store.clone();
        let target = target.clone();
        let mut release = release_rx.clone();
        let ready = ready_tx.clone();
        tokio::spawn(async move {
            ready.send("read").await.expect("announce read");
            release.changed().await.expect("release read");
            store.set_message_read_at_epoch(&target, true).await
        })
    };
    let moved = {
        let store = store.clone();
        let target = target.clone();
        let mut release = release_rx;
        let ready = ready_tx.clone();
        tokio::spawn(async move {
            ready.send("move").await.expect("announce move");
            release.changed().await.expect("release move");
            store
                .move_message_at_epoch(&target, "Trash", &MailboxMoveReceipt::default())
                .await
        })
    };
    drop(ready_tx);

    let mut announced = HashSet::new();
    for _ in 0..6 {
        announced.insert(ready_rx.recv().await.expect("all work classes announced"));
    }
    assert_eq!(
        announced,
        HashSet::from([
            "sync",
            "content",
            "attachment",
            "classification",
            "read",
            "move"
        ])
    );

    store
        .delete_account(deleted.id)
        .await
        .expect("delete account before releasing delayed work");
    release_tx.send(true).expect("release delayed work");

    assert!(sync
        .await
        .expect("join sync")
        .expect_err("late sync must fail")
        .to_string()
        .contains("account was removed"));
    assert!(!content
        .await
        .expect("join content")
        .expect("content result"));
    assert!(!attachment
        .await
        .expect("join attachment")
        .expect("attachment result"));
    assert_eq!(
        classify
            .await
            .expect("join classification")
            .expect("classification result"),
        0
    );
    assert!(!read.await.expect("join read").expect("read result"));
    assert!(
        !moved
            .await
            .expect("join move")
            .expect("move result")
            .source_published
    );

    drop(store);
    let reopened = Store::open(&database).await.expect("reopen store");
    assert!(reopened
        .account(deleted.id)
        .await
        .expect("read deleted account")
        .is_none());
    assert!(reopened
        .message_by_locator(deleted.id, "INBOX", 7)
        .await
        .expect("read deleted target")
        .is_none());
    assert!(reopened
        .message_by_locator(deleted.id, "INBOX", 8)
        .await
        .expect("read late sync target")
        .is_none());
    assert_eq!(
        reopened
            .message_by_locator(survivor.id, "INBOX", 7)
            .await
            .expect("read surviving target")
            .expect("surviving target exists")
            .subject,
        surviving_message.subject
    );
}

#[tokio::test]
async fn complete_stable_snapshot_replay_is_idempotent_across_restart() {
    let temporary = TempDir::new().expect("temporary directory");
    let database = temporary.path().join("stable-snapshot.sqlite");
    let account = account(0x01de_0017, "stable");
    let store = Store::open(&database).await.expect("open store");
    seed_mailbox(&store, &account, "INBOX", 404).await;
    let mut remote = vec![
        message(account.id, "INBOX", 1, "stable"),
        message(account.id, "INBOX", 2, "stable"),
        message(account.id, "INBOX", 3, "stable"),
    ];
    remote[0].is_read = true;
    remote[1].is_flagged = true;

    assert!(store
        .save_synced_messages_through_at_epoch(account.id, "INBOX", &remote, Some(3), 404)
        .await
        .expect("first complete snapshot")
        .is_empty());
    let uids = HashSet::from([1, 2, 3]);
    assert_eq!(
        store
            .reconcile_mailbox_uids(account.id, "INBOX", &uids)
            .await
            .expect("first reconcile"),
        0
    );

    let query = SearchQuery {
        account_ids: vec![account.id],
        mailbox: Some("INBOX".into()),
        ..SearchQuery::default()
    };
    let before = store.search(&query).await.expect("read first state");
    drop(store);

    let reopened = Store::open(&database).await.expect("reopen store");
    assert!(reopened
        .save_synced_messages_through_at_epoch(account.id, "INBOX", &remote, Some(3), 404)
        .await
        .expect("replay complete snapshot")
        .is_empty());
    assert_eq!(
        reopened
            .reconcile_mailbox_uids(account.id, "INBOX", &uids)
            .await
            .expect("replay reconcile"),
        0
    );
    let after = reopened.search(&query).await.expect("read replayed state");
    assert_eq!(after.len(), before.len());
    for (actual, expected) in after.iter().zip(&before) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.account_id, expected.account_id);
        assert_eq!(actual.mailbox, expected.mailbox);
        assert_eq!(actual.uid, expected.uid);
        assert_eq!(actual.mailbox_uid_validity, expected.mailbox_uid_validity);
        assert_eq!(actual.subject, expected.subject);
        assert_eq!(actual.is_read, expected.is_read);
        assert_eq!(actual.is_flagged, expected.is_flagged);
    }
    assert_eq!(
        reopened
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("read stable watermark"),
        Some(3)
    );
}

#[tokio::test]
async fn move_publication_matrix_preserves_exact_source_and_destination_epochs() {
    let store = Store::in_memory().await.expect("open store");
    let primary = account(0xac710, "primary");
    let other = account(0x07ae2, "other");
    seed_mailbox(&store, &primary, "INBOX", 10).await;
    seed_mailbox(&store, &other, "INBOX", 20).await;
    for (mailbox, epoch) in [("Archive", 30), ("Spam", 40), ("Trash", 50)] {
        store
            .save_mailbox_catalog_state(primary.id, mailbox, mailbox, epoch, 1, true)
            .await
            .expect("save destination epoch");
    }
    let unrelated = message(other.id, "INBOX", 1, "other");
    store
        .upsert_messages(std::slice::from_ref(&unrelated))
        .await
        .expect("save unrelated message");

    for (index, (destination, destination_epoch)) in [("Archive", 30), ("Spam", 40), ("Trash", 50)]
        .into_iter()
        .enumerate()
    {
        let source_uid = 10 + index as u32;
        let destination_uid = 100 + index as u32;
        let source = message(primary.id, "INBOX", source_uid, destination);
        store
            .upsert_messages(std::slice::from_ref(&source))
            .await
            .expect("save move source");
        let target = store
            .message_by_locator(primary.id, "INBOX", source_uid)
            .await
            .expect("read move source")
            .expect("move source exists");
        let receipt = MailboxMoveReceipt {
            destination: Some(MailboxMoveDestination {
                remote_mailbox: destination.into(),
                uid_validity: destination_epoch,
                uid: destination_uid,
            }),
        };
        let publication = store
            .move_message_at_epoch(&target, destination, &receipt)
            .await
            .expect("publish move");
        assert!(publication.source_published);
        assert!(publication.destination_mapped);
        assert!(store
            .message_by_locator(primary.id, "INBOX", source_uid)
            .await
            .expect("read removed source")
            .is_none());
        let moved = store
            .message_by_locator(primary.id, destination, destination_uid)
            .await
            .expect("read destination")
            .expect("destination exists");
        assert_eq!(moved.account_id, primary.id.to_string());
        assert_eq!(moved.mailbox_uid_validity, Some(destination_epoch));
    }

    // A provider can confirm an atomic MOVE without returning COPYUID. The
    // source must still disappear and become tombstoned, while Dakia waits for
    // sync to discover the destination instead of guessing its UID.
    let source = message(primary.id, "INBOX", 20, "receiptless");
    store
        .upsert_messages(std::slice::from_ref(&source))
        .await
        .expect("save receiptless source");
    let target = store
        .message_by_locator(primary.id, "INBOX", 20)
        .await
        .expect("read receiptless source")
        .expect("receiptless source exists");
    let receiptless = store
        .move_message_at_epoch(&target, "Archive", &MailboxMoveReceipt::default())
        .await
        .expect("publish receiptless move");
    assert!(receiptless.source_published);
    assert!(!receiptless.destination_mapped);

    // A COPYUID receipt from an old destination generation must never replace
    // the current destination row. The source is still safely removed because
    // the atomic remote MOVE itself was confirmed.
    let existing_destination = message(primary.id, "Trash", 999, "current-trash");
    let stale_source = message(primary.id, "INBOX", 21, "stale-receipt");
    store
        .upsert_messages(&[existing_destination.clone(), stale_source])
        .await
        .expect("save stale-receipt setup");
    let stale_target = store
        .message_by_locator(primary.id, "INBOX", 21)
        .await
        .expect("read stale-receipt source")
        .expect("stale-receipt source exists");
    let stale_receipt = MailboxMoveReceipt {
        destination: Some(MailboxMoveDestination {
            remote_mailbox: "Trash".into(),
            uid_validity: 49,
            uid: 999,
        }),
    };
    let publication = store
        .move_message_at_epoch(&stale_target, "Trash", &stale_receipt)
        .await
        .expect("publish stale destination receipt");
    assert!(publication.source_published);
    assert!(!publication.destination_mapped);
    assert_eq!(
        store
            .message_by_locator(primary.id, "Trash", 999)
            .await
            .expect("read protected destination")
            .expect("protected destination exists")
            .subject,
        existing_destination.subject
    );
    assert_eq!(
        store
            .message_by_locator(other.id, "INBOX", 1)
            .await
            .expect("read unrelated account")
            .expect("unrelated account remains")
            .subject,
        unrelated.subject
    );
}

#[tokio::test]
async fn remote_changes_and_delayed_local_completions_converge_by_immutable_locator() {
    let store = Store::in_memory().await.expect("open store");
    let primary = account(0x00c0_ac0e, "converge");
    let other = account(0x00c0_ac0f, "isolated");
    seed_mailbox(&store, &primary, "INBOX", 71).await;
    seed_mailbox(&store, &other, "INBOX", 81).await;
    let first = message(primary.id, "INBOX", 1, "first");
    let second = message(primary.id, "INBOX", 2, "second");
    let isolated = message(other.id, "INBOX", 1, "isolated");
    store
        .upsert_messages(&[first, second, isolated.clone()])
        .await
        .expect("seed messages");
    let delayed_first = store
        .message_by_locator(primary.id, "INBOX", 1)
        .await
        .expect("read first target")
        .expect("first target exists");
    let delayed_second = store
        .message_by_locator(primary.id, "INBOX", 2)
        .await
        .expect("read second target")
        .expect("second target exists");

    // Another device changes both flags, then moves UID 1 away. A delayed
    // completion for that now-absent locator is rejected, while a completion
    // for UID 2 can still publish without touching UID 1 or another account.
    store
        .update_mailbox_flags(primary.id, "INBOX", &[(1, true, true), (2, true, false)])
        .await
        .expect("apply remote flags");
    assert_eq!(
        store
            .reconcile_mailbox_uids(primary.id, "INBOX", &HashSet::from([2]))
            .await
            .expect("reconcile remote move"),
        1
    );
    assert!(!store
        .set_message_flagged_at_epoch(&delayed_first, false)
        .await
        .expect("publish absent completion"));
    assert!(store
        .set_message_flagged_at_epoch(&delayed_second, true)
        .await
        .expect("publish current completion"));

    let current = store
        .message_by_locator(primary.id, "INBOX", 2)
        .await
        .expect("read current message")
        .expect("current message exists");
    assert!(current.is_read);
    assert!(current.is_flagged);
    assert!(store
        .message_by_locator(primary.id, "INBOX", 1)
        .await
        .expect("read removed message")
        .is_none());
    assert_eq!(
        store
            .message_by_locator(other.id, "INBOX", 1)
            .await
            .expect("read isolated account")
            .expect("isolated message exists")
            .subject,
        isolated.subject
    );

    // A mailbox recreation recycles UID 2. The old captured completion is
    // rejected even though account, mailbox, UID, and stable id all match.
    store
        .save_mailbox_catalog_state(primary.id, "INBOX", "INBOX", 72, 1, true)
        .await
        .expect("replace mailbox epoch");
    let replacement = message(primary.id, "INBOX", 2, "replacement");
    store
        .replace_mailbox_messages(primary.id, "INBOX", &[replacement])
        .await
        .expect("publish replacement namespace");
    assert!(!store
        .set_message_read_at_epoch(&delayed_second, false)
        .await
        .expect("publish stale epoch completion"));
    let replacement = store
        .message_by_locator(primary.id, "INBOX", 2)
        .await
        .expect("read replacement")
        .expect("replacement exists");
    assert_eq!(replacement.subject, "replacement subject 2");
    assert_eq!(replacement.mailbox_uid_validity, Some(72));
    assert!(!replacement.is_read);
}
