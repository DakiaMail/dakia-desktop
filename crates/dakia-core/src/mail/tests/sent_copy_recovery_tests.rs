//! Durable recovery after SMTP accepts a message but IMAP cannot save Sent.

use super::*;
use crate::storage::{SendAttemptClaim, SendAttemptStatus};
use uuid::Uuid;

fn recovery_draft(account: &Account) -> ComposeMessage {
    ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: vec![],
        bcc: vec![],
        subject: "Missing Sent folder recovery".into(),
        body_text: "SMTP must receive this fictional message exactly once.".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
    }
}

async fn assert_replay_is_fenced(
    store: &Store,
    account: &Account,
    attempt_id: Uuid,
    payload_hash: &str,
    expected: SendAttemptStatus,
) {
    let claim = store
        .claim_send_attempt(account.id, attempt_id, payload_hash)
        .await
        .unwrap();
    assert_eq!(claim, SendAttemptClaim::Existing(expected));
    assert!(
        !matches!(claim, SendAttemptClaim::Claimed),
        "a recovered attempt must not enter the SMTP submission branch"
    );
}

#[tokio::test]
async fn missing_sent_folder_is_journaled_across_restart_without_resubmission() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("missing-sent-recovery.db");
    let store = Store::open(&database).await.unwrap();

    let smtp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imap_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = smtp_test_account(Security::Tls, smtp_listener.local_addr().unwrap().port());
    account.imap_host = "127.0.0.1".into();
    account.imap_port = imap_listener.local_addr().unwrap().port();
    account.imap_security = Security::Tls;
    store.save_account(&account).await.unwrap();

    let attempt_id = Uuid::new_v4();
    let payload_hash = "fictional-missing-sent-payload-v1";
    assert_eq!(
        store
            .claim_send_attempt(account.id, attempt_id, payload_hash)
            .await
            .unwrap(),
        SendAttemptClaim::Claimed
    );

    let smtp_server = tokio::spawn(async move {
        let (stream, _) = smtp_listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 fictional SMTP ready\r\n").await;
        smtp_expect_ehlo_and_auth(&mut connection, false).await;
        smtp_expect_envelope_until_data(&mut connection).await;
        let message = smtp_read_message(&mut connection).await;
        assert!(message.contains("Subject: Missing Sent folder recovery"));
        smtp_reply(
            &mut connection,
            "250 2.0.0 accepted once as missing-sent-fixture\r\n",
        )
        .await;
    });

    let imap_server = tokio::spawn(async move {
        let (stream, _) = imap_listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"sender@example.test\" \"secret\"\r\n"
        );
        smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        // The provider has no mailbox carrying the RFC SPECIAL-USE \\Sent
        // flag. Dakia falls back to the provider preset's Sent path, whose
        // APPEND then fails with TRYCREATE.
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0003 OK listed without Sent\r\n",
        )
        .await;
        let append = smtp_command(&mut connection).await;
        assert!(append.starts_with("D0004 APPEND \"Sent\" (\\Seen) {"));
        smtp_reply(
            &mut connection,
            "D0004 NO [TRYCREATE] no Sent mailbox exists\r\n",
        )
        .await;
    });

    let service = MailService::new(store.clone());
    let error = service
        .send_with_endpoints(
            &account,
            &recovery_draft(&account),
            "secret",
            smtp_test_endpoint(&account),
            Some(imap_test_endpoint(account.imap_port, true)),
            Duration::from_secs(2),
        )
        .await
        .expect_err("missing Sent folder must preserve successful SMTP delivery");
    smtp_server.await.unwrap();
    imap_server.await.unwrap();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::SentCopyFailed
    );
    store
        .complete_send_attempt(
            account.id,
            attempt_id,
            payload_hash,
            SendAttemptStatus::SentCopyFailed,
        )
        .await
        .unwrap();

    // Simulate a process restart. Replaying the same attempt returns its
    // terminal outcome before any SMTP or IMAP future can be constructed.
    drop(service);
    drop(store);
    let reopened = Store::open(&database).await.unwrap();
    assert_replay_is_fenced(
        &reopened,
        &account,
        attempt_id,
        payload_hash,
        SendAttemptStatus::SentCopyFailed,
    )
    .await;

    // Crash-window and ambiguous-delivery states are equally non-resendable.
    let pending_attempt = Uuid::new_v4();
    reopened
        .claim_send_attempt(account.id, pending_attempt, "pending-after-restart")
        .await
        .unwrap();
    let uncertain_attempt = Uuid::new_v4();
    reopened
        .claim_send_attempt(account.id, uncertain_attempt, "uncertain-after-restart")
        .await
        .unwrap();
    reopened
        .complete_send_attempt(
            account.id,
            uncertain_attempt,
            "uncertain-after-restart",
            SendAttemptStatus::DeliveryUncertain,
        )
        .await
        .unwrap();
    drop(reopened);

    let restarted_again = Store::open(&database).await.unwrap();
    assert_replay_is_fenced(
        &restarted_again,
        &account,
        pending_attempt,
        "pending-after-restart",
        SendAttemptStatus::Pending,
    )
    .await;
    assert_replay_is_fenced(
        &restarted_again,
        &account,
        uncertain_attempt,
        "uncertain-after-restart",
        SendAttemptStatus::DeliveryUncertain,
    )
    .await;
}
