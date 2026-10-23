//! Public TLS sync recovery for malformed IMAP responses.
//!
//! The lower-level transcript tests cover parser branches. This fixture keeps
//! the production TCP/TLS connector, authentication, snapshot staging, and
//! durable Store in the loop so a malformed response cannot look like an
//! authoritative empty or advanced mailbox after a restart.

use super::*;
use tokio::{
    io::{split, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[derive(Clone, Copy, Debug)]
enum SyncPass {
    Baseline,
    WrongTag,
    TruncatedLiteral,
    MalformedLiteralLength,
    OversizedLiteral,
    Retry,
}

const HEADER_FIELDS: &str = "BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]";

fn headers_for(uid: u32) -> String {
    format!(
        "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Public sync fixture {uid}\r\nMessage-ID: <public-sync-{uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
    )
}

fn select_response(pass: SyncPass) -> String {
    let exists = if matches!(pass, SyncPass::Baseline) {
        1
    } else {
        2
    };
    let uid_next = if matches!(pass, SyncPass::Baseline) {
        43
    } else {
        44
    };
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT {uid_next}] next\r\n"
    )
}

fn flags_response(pass: SyncPass, tag: &str) -> String {
    let rows = if matches!(pass, SyncPass::Baseline) {
        "* 1 FETCH (UID 42 FLAGS ())\r\n"
    } else {
        "* 1 FETCH (UID 42 FLAGS ())\r\n* 2 FETCH (UID 43 FLAGS ())\r\n"
    };
    format!("{rows}{tag} OK flags\r\n")
}

fn metadata_response(tag: &str, uid: u32) -> String {
    let headers = headers_for(uid);
    format!(
        "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" RFC822.SIZE 5 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1) {HEADER_FIELDS} {{{}}}\r\n{headers})\r\n{tag} OK metadata\r\n",
        headers.len()
    )
}

fn snippet_response(tag: &str, uid: u32) -> String {
    format!("* 1 FETCH (UID {uid} BODY[]<0> {{5}}\r\nhello)\r\n{tag} OK preview\r\n")
}

async fn serve_sync_pass(listener: TcpListener, passes: Vec<SyncPass>) -> Vec<Vec<String>> {
    let acceptor = smtp_test_acceptor();
    let mut transcripts = Vec::with_capacity(passes.len());
    for pass in passes {
        let (stream, _) = listener.accept().await.expect("accept public IMAP client");
        let stream = acceptor.accept(stream).await.expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional public IMAP ready\r\n")
            .await
            .expect("write greeting");
        write.flush().await.expect("flush greeting");

        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n")
            } else if command == "LOGIN \"reader@example.test\" \"public sync secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "LIST \"\" \"*\"" {
                format!("* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK listed\r\n")
            } else if command == "SELECT \"INBOX\"" {
                format!("{}{tag} OK selected\r\n", select_response(pass))
            } else if command
                == if matches!(pass, SyncPass::Baseline) {
                    "FETCH 1:1 (UID FLAGS)"
                } else {
                    "FETCH 1:2 (UID FLAGS)"
                }
            {
                flags_response(pass, &tag)
            } else if command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE") {
                metadata_response(&tag, 42)
            } else if command.starts_with("UID FETCH 43 (FLAGS INTERNALDATE") {
                match pass {
                    SyncPass::WrongTag => {
                        write
                            .write_all(b"D9999 OK unrelated command\r\n")
                            .await
                            .expect("write wrong tag");
                        write.flush().await.expect("flush wrong tag");
                        break;
                    }
                    SyncPass::TruncatedLiteral => {
                        let headers = headers_for(43);
                        let truncated = &headers.as_bytes()[..headers.len() / 2];
                        write
                            .write_all(
                                format!(
                                    "* 2 FETCH (UID 43 FLAGS () {HEADER_FIELDS} {{{}}}\r\n",
                                    headers.len()
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("declare literal before truncation");
                        write
                            .write_all(truncated)
                            .await
                            .expect("write partial literal");
                        write.flush().await.expect("flush partial literal");
                        break;
                    }
                    SyncPass::MalformedLiteralLength => format!(
                        "* 2 FETCH (UID 43 FLAGS () {HEADER_FIELDS} {{not-a-length}}\r\n{tag} OK malformed metadata\r\n"
                    ),
                    SyncPass::OversizedLiteral => format!(
                        "* 2 FETCH (UID 43 FLAGS () {HEADER_FIELDS} {{{}}}\r\n",
                        MAX_MIME_HEADER_BYTES + 1
                    ),
                    SyncPass::Baseline | SyncPass::Retry => metadata_response(&tag, 43),
                }
            } else if command == "UID FETCH 42 (BODY.PEEK[]<0.8192>)" {
                snippet_response(&tag, 42)
            } else if command == "UID FETCH 43 (BODY.PEEK[]<0.8192>)" {
                snippet_response(&tag, 43)
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected public sync command for {pass:?}: {command}");
            };
            write
                .write_all(response.as_bytes())
                .await
                .expect("write fictional IMAP response");
            write.flush().await.expect("flush fictional IMAP response");
            if matches!(
                pass,
                SyncPass::MalformedLiteralLength | SyncPass::OversizedLiteral
            ) && command.starts_with("UID FETCH 43 (FLAGS INTERNALDATE")
            {
                break;
            }
        }
        transcripts.push(transcript);
    }
    transcripts
}

async fn assert_failed_sync_survives_reopen(
    database: &std::path::Path,
    account: &Account,
    message_before: &serde_json::Value,
    catalog_before: &(i64, i64, bool, Option<i64>, Option<String>),
    endpoint: ImapEndpoint,
    label: &str,
) -> Store {
    let store = Store::open(database)
        .await
        .expect("reopen profile before fault");
    let service = MailService::new(store.clone());
    let error = service
        .sync_inbox_with_imap_endpoint_for_test(account, 50, endpoint)
        .await
        .expect_err(label);
    assert!(
        !error.to_string().is_empty(),
        "{label} must surface a sync error rather than treating malformed data as authoritative"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .message_by_locator(account.id, "INBOX", 42)
                .await
                .expect("read known message")
                .expect("known message remains visible"),
        )
        .expect("serialize known message"),
        *message_before,
        "{label} must not alter the committed message"
    );
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 43)
            .await
            .expect("read uncommitted message")
            .is_none(),
        "{label} must not publish a partial new message"
    );
    assert_eq!(
        store
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("read durable sync watermark"),
        Some(42),
        "{label} must not advance the durable sync watermark"
    );
    let state = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .expect("read catalog state")
        .expect("baseline catalog state");
    assert_eq!(
        (
            state.uid_validity,
            state.remote_total,
            state.historical_complete,
            state.uid_next,
            state.highest_modseq,
        ),
        *catalog_before,
        "{label} must not publish an incomplete mailbox inventory"
    );
    drop(service);
    drop(store);

    let reopened = Store::open(database)
        .await
        .expect("reopen profile after fault");
    assert_eq!(
        reopened
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("read reopened watermark"),
        Some(42),
        "{label} must remain fenced after restart"
    );
    reopened
}

#[tokio::test]
async fn ms_b_003_b_011_public_tls_sync_malformed_responses_preserve_state_then_retry() {
    let directory = tempfile::tempdir().expect("temporary fictional profile");
    let database = directory.path().join("public-imap-recovery.sqlite");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional public IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let passes = vec![
        SyncPass::Baseline,
        SyncPass::WrongTag,
        SyncPass::TruncatedLiteral,
        SyncPass::MalformedLiteralLength,
        SyncPass::OversizedLiteral,
        SyncPass::Retry,
    ];
    let server = tokio::spawn(serve_sync_pass(listener, passes));

    let store = Store::open(&database).await.expect("open profile");
    let mut account = imap_test_account(Security::Tls, port);
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    store.save_account(&account).await.expect("save account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "public sync secret")
        .await
        .expect("save fictional credential");
    service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, imap_test_endpoint(port, true))
        .await
        .expect("establish the committed baseline through public TLS sync");

    let message_before = serde_json::to_value(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read baseline message")
            .expect("baseline message"),
    )
    .expect("serialize baseline message");
    let baseline = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .expect("read baseline catalog state")
        .expect("baseline catalog state");
    let catalog_before = (
        baseline.uid_validity,
        baseline.remote_total,
        baseline.historical_complete,
        baseline.uid_next,
        baseline.highest_modseq,
    );
    assert_eq!(
        store
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("read baseline watermark"),
        Some(42)
    );
    drop(service);
    drop(store);

    for label in [
        "wrong tagged response",
        "truncated declared literal",
        "malformed declared literal length",
        "oversized declared literal",
    ] {
        let store = assert_failed_sync_survives_reopen(
            &database,
            &account,
            &message_before,
            &catalog_before,
            imap_test_endpoint(port, true),
            label,
        )
        .await;
        drop(store);
    }

    let reopened = Store::open(&database)
        .await
        .expect("open profile for valid retry");
    let retry = MailService::new(reopened.clone())
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, imap_test_endpoint(port, true))
        .await
        .expect("a valid later response must repair the fenced UID");
    assert_eq!(retry.synced_count, 1);
    assert!(
        reopened
            .message_by_locator(account.id, "INBOX", 43)
            .await
            .expect("read repaired message")
            .is_some(),
        "the valid retry must publish the previously fenced message"
    );
    assert_eq!(
        reopened
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("read repaired watermark"),
        Some(43)
    );
    drop(reopened);

    let transcripts = server.await.expect("public sync server");
    assert_eq!(transcripts.len(), 6);
    for (index, transcript) in transcripts.iter().enumerate() {
        assert!(
            transcript.iter().any(|command| command == "CAPABILITY"),
            "pass {index} must traverse the production TLS capability path"
        );
        assert!(
            transcript
                .iter()
                .any(|command| command == "LOGIN \"reader@example.test\" \"public sync secret\""),
            "pass {index} must authenticate the fictional account"
        );
        assert!(
            transcript
                .iter()
                .any(|command| command.starts_with("FETCH ")),
            "pass {index} must request the real snapshot inventory"
        );
    }
}
