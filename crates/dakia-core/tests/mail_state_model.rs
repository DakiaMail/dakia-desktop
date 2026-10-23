use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use dakia_core::{provider, Account, AccountDraft, AccountId, MailSummary, Store};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use tempfile::TempDir;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedMessage {
    subject: String,
    is_read: bool,
    is_flagged: bool,
}

type Locator = (AccountId, String, u32);

fn account(label: &str) -> Account {
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
    account.id = match label {
        "alpha" => AccountId::from_u128(0xa11a),
        "bravo" => AccountId::from_u128(0xb2a0),
        _ => panic!("unknown state-model account: {label}"),
    };
    account
}

fn message(account_id: AccountId, mailbox: &str, uid: u32, label: &str) -> MailSummary {
    MailSummary {
        id: format!("{account_id}-{mailbox}-{uid}-{label}"),
        account_id: account_id.to_string(),
        mailbox: mailbox.into(),
        uid: i64::from(uid),
        mailbox_uid_validity: None,
        message_id: Some(format!("<{label}-{uid}@example.test>")),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: format!("{account_id}-{label}-{uid}"),
        subject: format!("{label} subject {uid}"),
        from_name: Some("State model sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: format!("{label}@example.test"),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("fixed timestamp")
            + ChronoDuration::seconds(i64::from(uid)),
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

async fn assert_matches_model(
    store: &Store,
    expected: &BTreeMap<Locator, ExpectedMessage>,
    absent: &BTreeSet<Locator>,
) {
    for ((account_id, mailbox, uid), state) in expected {
        let actual = store
            .message_by_locator(*account_id, mailbox, *uid)
            .await
            .expect("read model locator")
            .unwrap_or_else(|| panic!("missing model locator {account_id}/{mailbox}/{uid}"));
        assert_eq!(actual.account_id, account_id.to_string());
        assert_eq!(actual.mailbox, *mailbox);
        assert_eq!(actual.uid, i64::from(*uid));
        assert_eq!(actual.subject, state.subject);
        assert_eq!(actual.is_read, state.is_read);
        assert_eq!(actual.is_flagged, state.is_flagged);
    }
    for (account_id, mailbox, uid) in absent {
        assert!(
            store
                .message_by_locator(*account_id, mailbox, *uid)
                .await
                .expect("read absent model locator")
                .is_none(),
            "removed model locator reappeared: {account_id}/{mailbox}/{uid}"
        );
    }
}

/// Runs the same public operations used by sync and mutation code against a
/// small reference model. The fixed sequence deliberately reopens SQLite
/// throughout the run, so a passing in-memory result cannot hide missing
/// commits, wrong-account writes, or messages resurrected by stale fetches.
#[tokio::test]
async fn deterministic_mail_state_sequence_survives_restarts_without_cross_account_leaks() {
    let temporary = TempDir::new().expect("temporary directory");
    let database = temporary.path().join("dakia.db");
    let accounts = [account("alpha"), account("bravo")];
    let mut store = Store::open(&database).await.expect("open state store");
    let mut expected: BTreeMap<Locator, ExpectedMessage> = BTreeMap::new();
    let mut absent = BTreeSet::new();

    for account in &accounts {
        store.save_account(account).await.expect("save account");
        let label = account.email.split('@').next().expect("account label");
        let initial: Vec<_> = (1..=6)
            .map(|uid| message(account.id, "INBOX", uid, label))
            .collect();
        store
            .save_synced_messages(account.id, "INBOX", &initial)
            .await
            .expect("seed account mailbox");
        for item in initial {
            expected.insert(
                (account.id, "INBOX".into(), item.uid as u32),
                ExpectedMessage {
                    subject: item.subject,
                    is_read: false,
                    is_flagged: false,
                },
            );
        }
    }

    let mut seed = 0x5eed_cafe_u64;
    let mut stale_sources = Vec::new();
    for step in 0..72_u32 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let keys: Vec<_> = expected.keys().cloned().collect();
        let locator = keys[(seed as usize) % keys.len()].clone();
        let account_message_count = keys.iter().filter(|key| key.0 == locator.0).count();
        let operation = if account_message_count > 2 {
            ((seed >> 32) % 4) as u8
        } else {
            ((seed >> 32) % 3) as u8
        };

        match operation {
            0 => {
                let actual = store
                    .message_by_locator(locator.0, &locator.1, locator.2)
                    .await
                    .expect("locate message for read mutation")
                    .expect("message for read mutation");
                let next = !expected[&locator].is_read;
                store
                    .set_message_read(&actual.id, next)
                    .await
                    .expect("set read state");
                expected
                    .get_mut(&locator)
                    .expect("model read target")
                    .is_read = next;
            }
            1 => {
                let actual = store
                    .message_by_locator(locator.0, &locator.1, locator.2)
                    .await
                    .expect("locate message for flag mutation")
                    .expect("message for flag mutation");
                let next = !expected[&locator].is_flagged;
                store
                    .set_message_flagged(&actual.id, next)
                    .await
                    .expect("set flagged state");
                expected
                    .get_mut(&locator)
                    .expect("model flag target")
                    .is_flagged = next;
            }
            2 => {
                let destination = if locator.1 == "Archive" {
                    "Trash"
                } else {
                    "Archive"
                };
                let destination_uid = 1_000 + step;
                let old = store
                    .message_by_locator(locator.0, &locator.1, locator.2)
                    .await
                    .expect("locate message before move")
                    .expect("message before move");
                store
                    .move_message(
                        locator.0,
                        &locator.1,
                        locator.2,
                        destination,
                        Some(destination_uid),
                    )
                    .await
                    .expect("move model message");
                let state = expected.remove(&locator).expect("model move source");
                expected.insert((locator.0, destination.into(), destination_uid), state);
                absent.insert(locator);
                stale_sources.push(old);
            }
            _ => {
                // Reconcile one mailbox against a complete provider snapshot.
                // Exactly one selected locator is absent from that snapshot.
                let remote: HashSet<u32> = expected
                    .keys()
                    .filter(|key| key.0 == locator.0 && key.1 == locator.1 && *key != &locator)
                    .map(|key| key.2)
                    .collect();
                store
                    .reconcile_mailbox_uids(locator.0, &locator.1, &remote)
                    .await
                    .expect("reconcile model mailbox");
                expected.remove(&locator).expect("model reconcile target");
                absent.insert(locator);
            }
        }

        if step % 6 == 5 {
            drop(store);
            store = Store::open(&database).await.expect("reopen state store");
            assert_matches_model(&store, &expected, &absent).await;
        }
    }

    // A provider response fetched before a move completed must not recreate
    // any source locator. Tombstones are durable across all prior reopens.
    store
        .upsert_messages(&stale_sources)
        .await
        .expect("publish stale provider responses");
    for stale in &stale_sources {
        assert!(
            store
                .message_by_locator(
                    AccountId::parse_str(&stale.account_id).expect("stale account id"),
                    &stale.mailbox,
                    stale.uid as u32,
                )
                .await
                .expect("check stale source")
                .is_none(),
            "stale response resurrected {}/{}/{}",
            stale.account_id,
            stale.mailbox,
            stale.uid
        );
    }

    let deleted_id = accounts[0].id;
    store
        .delete_account(deleted_id)
        .await
        .expect("delete first account");
    let deleted_locators: Vec<_> = expected
        .keys()
        .filter(|key| key.0 == deleted_id)
        .cloned()
        .collect();
    expected.retain(|key, _| key.0 != deleted_id);
    absent.extend(deleted_locators);
    let stale_after_delete = message(deleted_id, "INBOX", 99, "deleted");
    let error = store
        .save_synced_messages(deleted_id, "INBOX", &[stale_after_delete])
        .await
        .expect_err("late sync must not recreate a deleted account");
    assert!(error.to_string().contains("account was removed"));

    for _ in 0..3 {
        drop(store);
        store = Store::open(&database)
            .await
            .expect("repeat reopen state store");
        assert!(store
            .account(deleted_id)
            .await
            .expect("read deleted account")
            .is_none());
        assert_matches_model(&store, &expected, &absent).await;
        assert!(store
            .accounts()
            .await
            .expect("read surviving accounts")
            .iter()
            .all(|account| account.id == accounts[1].id));
    }
}
