use super::*;
use crate::{account::AccountDraft, provider};

fn test_account_for_id(account_id: AccountId) -> Account {
    let mut account = AccountDraft {
        email: format!("{account_id}@example.test"),
        display_name: "Wire identity test".into(),
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
    account.id = account_id;
    account
}

async fn store_with_account() -> (Store, AccountId) {
    let store = Store::in_memory().await.unwrap();
    let account_id = uuid::Uuid::new_v4();
    let account = test_account_for_id(account_id);
    store.save_account(&account).await.unwrap();
    (store, account_id)
}

async fn save_remote(store: &Store, account_id: AccountId, mailbox: &str, remote: &str) {
    store
        .save_mailbox_catalog_state(account_id, mailbox, remote, 1, 0, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn remote_catalogue_lookup_preserves_ascii_wire_case_outside_inbox() {
    let (store, account_id) = store_with_account().await;
    save_remote(&store, account_id, "ArchiveUpper", "Archive").await;
    save_remote(&store, account_id, "ArchiveLower", "archive").await;

    let upper = store
        .mailbox_catalog_states_for_remote(account_id, "Archive")
        .await
        .unwrap();
    let lower = store
        .mailbox_catalog_states_for_remote(account_id, "archive")
        .await
        .unwrap();
    assert_eq!(upper.len(), 1);
    assert_eq!(upper[0].mailbox, "ArchiveUpper");
    assert_eq!(lower.len(), 1);
    assert_eq!(lower[0].mailbox, "ArchiveLower");
}

#[tokio::test]
async fn remote_catalogue_lookup_preserves_modified_utf7_base64_case() {
    let (store, account_id) = store_with_account().await;
    // Both are canonical modified UTF-7. `&AKA-` is U+00A0 and `&AkA-` is
    // U+0240. ASCII case folding aliases them even though the wire mailboxes
    // and decoded logical names are distinct.
    save_remote(&store, account_id, "NoBreakSpace", "&AKA-").await;
    save_remote(&store, account_id, "ZWithSwashTail", "&AkA-").await;

    let first = store
        .mailbox_catalog_states_for_remote(account_id, "&AKA-")
        .await
        .unwrap();
    let second = store
        .mailbox_catalog_states_for_remote(account_id, "&AkA-")
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].mailbox, "NoBreakSpace");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].mailbox, "ZWithSwashTail");
}

#[tokio::test]
async fn only_the_reserved_inbox_name_is_case_insensitive() {
    let (store, account_id) = store_with_account().await;
    save_remote(&store, account_id, "INBOX", "Inbox").await;
    save_remote(&store, account_id, "InboxChild", "Inbox/Child").await;

    for spelling in ["INBOX", "inbox", "InBoX"] {
        let states = store
            .mailbox_catalog_states_for_remote(account_id, spelling)
            .await
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].mailbox, "INBOX");
    }
    assert!(store
        .mailbox_catalog_states_for_remote(account_id, "inbox/child")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .mailbox_catalog_states_for_remote(account_id, "Inbox/Child")
            .await
            .unwrap()
            .len(),
        1
    );
}
