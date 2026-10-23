//! Native action routing regressions. These invoke the production command
//! helper with a real Store and real account-operation lock; invalid delayed
//! targets must be rejected before `MailService` can open a provider session.
use super::*;
use chrono::Utc;

const CURRENT_EPOCH: u32 = 77;
const CHANGED_MESSAGE: &str = "This message changed. Refresh the mailbox before trying again.";

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("mutation target validation must run before classification")
    }
}

fn state(store: Store) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir: PathBuf::new(),
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "mail-action-identity-test".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn test_account(email: &str) -> Account {
    AccountDraft {
        email: email.into(),
        display_name: "Action identity test".into(),
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
    .into_account(provider::by_id("custom").expect("custom provider"))
}

fn message(account_id: Uuid) -> MailSummary {
    MailSummary {
        id: "message-current".into(),
        account_id: account_id.to_string(),
        mailbox: "INBOX".into(),
        uid: 42,
        mailbox_uid_validity: None,
        message_id: Some("<message-current@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "thread-current".into(),
        subject: "Current message".into(),
        from_name: None,
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: String::new(),
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

async fn state_with_current_target() -> (Arc<AppState>, Account, Account, MailSummary) {
    let store = Store::in_memory().await.expect("in-memory store");
    let account = test_account("owner@example.test");
    let other_account = test_account("other@example.test");
    store
        .save_account(&account)
        .await
        .expect("save owner account");
    store
        .save_account(&other_account)
        .await
        .expect("save other account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", CURRENT_EPOCH, 1, true)
        .await
        .expect("save mailbox epoch");
    let message = message(account.id);
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("save current message");
    let current = store
        .message(&message.id)
        .await
        .expect("read current message")
        .expect("current message exists");
    assert_eq!(current.mailbox_uid_validity, Some(i64::from(CURRENT_EPOCH)));
    (state(store), account, other_account, current)
}

fn actions() -> [MailboxAction; 5] {
    [
        MailboxAction::Archive,
        MailboxAction::Spam,
        MailboxAction::NotSpam,
        MailboxAction::Trash,
        MailboxAction::Delete,
    ]
}

#[tokio::test]
async fn every_mailbox_action_rejects_a_delayed_cross_account_mailbox_uid_or_message_target() {
    let (state, account, other_account, current) = state_with_current_target().await;
    let candidates = [
        (
            other_account.id,
            current.mailbox.clone(),
            current.uid as u32,
            Some(current.id.clone()),
            Some(i64::from(CURRENT_EPOCH)),
            "another account",
        ),
        (
            account.id,
            "Archive".into(),
            current.uid as u32,
            Some(current.id.clone()),
            Some(i64::from(CURRENT_EPOCH)),
            "another mailbox",
        ),
        (
            account.id,
            current.mailbox.clone(),
            current.uid as u32 + 1,
            Some(current.id.clone()),
            Some(i64::from(CURRENT_EPOCH)),
            "another UID",
        ),
        (
            account.id,
            current.mailbox.clone(),
            current.uid as u32,
            Some("message-replaced-after-menu-open".into()),
            Some(i64::from(CURRENT_EPOCH)),
            "another message ID",
        ),
    ];

    for action in actions() {
        for (account_id, mailbox, uid, message_id, epoch, mismatch) in &candidates {
            let error = apply_mailbox_action_inner(
                &state,
                *account_id,
                mailbox.clone(),
                *uid,
                action,
                message_id.clone(),
                *epoch,
            )
            .await
            .expect_err("delayed native menu dispatch must be fenced before IMAP");
            assert_eq!(error, CHANGED_MESSAGE, "{action:?} with {mismatch}");
        }
    }
}

#[tokio::test]
async fn every_mailbox_action_rejects_a_stale_or_missing_epoch_before_provider_work() {
    let (state, account, _other_account, current) = state_with_current_target().await;

    for expected_epoch in [None, Some(0), Some(i64::from(CURRENT_EPOCH) - 1)] {
        for action in actions() {
            let error = apply_mailbox_action_inner(
                &state,
                account.id,
                current.mailbox.clone(),
                current.uid as u32,
                action,
                Some(current.id.clone()),
                expected_epoch,
            )
            .await
            .expect_err("stale menu epochs must fail before IMAP");
            assert_eq!(error, CHANGED_MESSAGE, "{action:?} with {expected_epoch:?}");
        }
    }
}
