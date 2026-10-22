//! Public TLS realtime recovery coverage.
//!
//! These fixtures use the real connector and IDLE state machine. The supplied
//! endpoint is loopback-only and trusts the test CA; production still builds
//! its endpoint from WebPKI roots.
//!
//! Physical sleep/wake remains a separate acceptance lane. The user declined
//! an interrupting physical sleep test; the production IDLE renewal deadline
//! is exercised here against a bounded loopback TLS server.

use super::*;
use std::collections::VecDeque;
use tokio::{
    io::{split, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
};

#[derive(Clone, Copy, Debug)]
enum RealtimeScript {
    Disconnect,
    Renewal,
    NoopNoise,
    Arrival,
    SeenArrival,
    FlagChange,
    Expunge,
    Poll,
}

fn realtime_headers_for_uid(uid: u32) -> String {
    format!(
        "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Realtime TLS arrival {uid}\r\nMessage-ID: <{uid}@realtime-public.example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
    )
}

fn realtime_headers() -> &'static str {
    "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Realtime TLS arrival\r\nMessage-ID: <42@realtime-public.example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
}

fn realtime_identity(script: RealtimeScript, after_idle: bool) -> String {
    let (exists, uid_next) = match script {
        RealtimeScript::Disconnect
        | RealtimeScript::Renewal
        | RealtimeScript::NoopNoise
        | RealtimeScript::Poll => (0, 43),
        RealtimeScript::Arrival | RealtimeScript::SeenArrival if !after_idle => (0, 43),
        RealtimeScript::Arrival | RealtimeScript::SeenArrival => (1, 44),
        RealtimeScript::FlagChange => (1, 44),
        // IMAP UIDs never move backward when the only message is expunged.
        RealtimeScript::Expunge if after_idle => (0, 44),
        RealtimeScript::Expunge => (1, 44),
    };
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT {uid_next}] next\r\n"
    )
}

async fn start_realtime_server(
    script: RealtimeScript,
) -> (ImapEndpoint, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional public realtime listener");
    let endpoint = imap_test_endpoint(listener.local_addr().unwrap().port(), true);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept realtime client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional realtime TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let mut selects = 0_u8;
        let mut after_idle = false;
        write
            .write_all(b"* OK fictional public IMAP ready\r\n")
            .await
            .expect("write realtime greeting");
        write.flush().await.expect("flush realtime greeting");

        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "CAPABILITY" {
                let capability = if matches!(script, RealtimeScript::Poll) {
                    "IMAP4rev1"
                } else {
                    "IMAP4rev1 IDLE"
                };
                format!("* CAPABILITY {capability}\r\n{tag} OK capability\r\n")
            } else if command == "LOGIN \"reader@example.test\" \"realtime secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "SELECT INBOX" {
                selects += 1;
                format!(
                    "{}{tag} OK selected\r\n",
                    realtime_identity(script, after_idle)
                )
            } else if command == "FETCH 1:1 (UID FLAGS)" {
                assert!(
                    !matches!(script, RealtimeScript::Disconnect | RealtimeScript::Poll)
                        || after_idle,
                    "an empty mailbox must not have an inventory page"
                );
                let flags = if (matches!(script, RealtimeScript::FlagChange) && after_idle)
                    || matches!(
                        script,
                        RealtimeScript::SeenArrival | RealtimeScript::Expunge
                    ) {
                    "\\Seen"
                } else {
                    ""
                };
                format!("* 1 FETCH (UID 42 FLAGS ({flags}))\r\n{tag} OK flags\r\n")
            } else if command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE") {
                assert!(
                    matches!(
                        script,
                        RealtimeScript::Arrival | RealtimeScript::SeenArrival
                    ) && after_idle,
                    "only the arrival refresh may fetch new metadata"
                );
                let headers = realtime_headers();
                let flags = if matches!(script, RealtimeScript::SeenArrival) {
                    "\\Seen"
                } else {
                    ""
                };
                format!(
                    "* 1 FETCH (UID 42 FLAGS ({flags}) INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n{headers})\r\n{tag} OK metadata\r\n",
                    headers.len()
                )
            } else if command == "IDLE" {
                if matches!(script, RealtimeScript::Renewal | RealtimeScript::NoopNoise) {
                    assert!(selects >= 3, "IDLE starts only after a complete snapshot");
                } else {
                    assert_eq!(selects, 3, "IDLE starts only after a complete snapshot");
                }
                assert!(
                    !matches!(script, RealtimeScript::Poll),
                    "polling fallback must not enter IDLE"
                );
                write
                    .write_all(b"+ idling\r\n")
                    .await
                    .expect("write IDLE continuation");
                match script {
                    RealtimeScript::Disconnect => {
                        write
                            .write_all(b"* BYE fictional network drop\r\n")
                            .await
                            .expect("write IDLE disconnect");
                        write.flush().await.expect("flush IDLE disconnect");
                        break;
                    }
                    RealtimeScript::Renewal => {}
                    RealtimeScript::NoopNoise => {
                        write
                            .write_all(b"* 1 FETCH (UID 42 FLAGS ())\r\n")
                            .await
                            .expect("write no-op invalidation");
                    }
                    RealtimeScript::Arrival | RealtimeScript::SeenArrival => {
                        write
                            .write_all(b"* 1 EXISTS\r\n")
                            .await
                            .expect("write arrival invalidation");
                    }
                    RealtimeScript::FlagChange => {
                        write
                            .write_all(b"* 1 FETCH (UID 42 FLAGS (\\Seen))\r\n")
                            .await
                            .expect("write flag invalidation");
                    }
                    RealtimeScript::Expunge => {
                        write
                            .write_all(b"* 1 EXPUNGE\r\n* 0 EXISTS\r\n")
                            .await
                            .expect("write expunge invalidation");
                    }
                    RealtimeScript::Poll => unreachable!(),
                }
                write.flush().await.expect("flush IDLE continuation");
                let mut done = String::new();
                let count = read.read_line(&mut done).await.expect("read IDLE DONE");
                assert_ne!(count, 0, "client must terminate IDLE");
                assert_eq!(done, "DONE\r\n");
                transcript.push("DONE".into());
                write
                    .write_all(format!("{tag} OK IDLE completed\r\n").as_bytes())
                    .await
                    .expect("complete IDLE");
                write.flush().await.expect("flush IDLE completion");
                after_idle = true;
                continue;
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write realtime logout");
                write.flush().await.expect("flush realtime logout");
                break;
            } else {
                panic!("unexpected public realtime command for {script:?}: {command}");
            };
            write
                .write_all(response.as_bytes())
                .await
                .expect("write realtime response");
            write.flush().await.expect("flush realtime response");
        }
        transcript
    });
    (endpoint, server)
}

async fn start_bounded_arrival_server(
    expected_metadata: impl IntoIterator<Item = u32>,
    idle_before_fetch: bool,
) -> (ImapEndpoint, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind bounded arrival listener");
    let endpoint = imap_test_endpoint(listener.local_addr().unwrap().port(), true);
    let expected_metadata = expected_metadata.into_iter().collect::<VecDeque<_>>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("accept bounded arrival client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept bounded arrival TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let mut expected_metadata = expected_metadata;
        let mut arrived = !idle_before_fetch;
        write
            .write_all(b"* OK fictional bounded IMAP ready\r\n")
            .await
            .expect("write bounded greeting");
        write.flush().await.expect("flush bounded greeting");

        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let identity = if arrived {
                "* 51 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 52] next\r\n"
            } else {
                "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 1] next\r\n"
            };
            let response = if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1 IDLE\r\n{tag} OK capability\r\n")
            } else if command == "LOGIN \"reader@example.test\" \"realtime secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "SELECT INBOX" {
                format!("{identity}{tag} OK selected\r\n")
            } else if command == "FETCH 1:51 (UID FLAGS)" {
                assert!(arrived, "only the post-arrival snapshot has 51 UIDs");
                let flags = (1..=51)
                    .map(|uid| format!("* {uid} FETCH (UID {uid} FLAGS ())\r\n"))
                    .collect::<String>();
                format!("{flags}{tag} OK flags\r\n")
            } else if command.starts_with("UID FETCH ") {
                let uid = command
                    .strip_prefix("UID FETCH ")
                    .and_then(|rest| rest.split_once(' ').map(|(uid, _)| uid))
                    .and_then(|uid| uid.parse::<u32>().ok())
                    .expect("metadata UID");
                assert_eq!(
                    expected_metadata.pop_front(),
                    Some(uid),
                    "metadata must advance from the oldest missing UID"
                );
                let headers = realtime_headers_for_uid(uid);
                format!(
                    "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE LIST-ID PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n{headers})\r\n{tag} OK metadata\r\n",
                    headers.len()
                )
            } else if command == "IDLE" {
                assert!(idle_before_fetch && !arrived, "only the first cycle idles");
                write
                    .write_all(b"+ idling\r\n* 51 EXISTS\r\n")
                    .await
                    .expect("write bounded arrival invalidation");
                write
                    .flush()
                    .await
                    .expect("flush bounded arrival invalidation");
                let mut done = String::new();
                assert_ne!(read.read_line(&mut done).await.expect("read DONE"), 0);
                assert_eq!(done, "DONE\r\n");
                write
                    .write_all(format!("{tag} OK IDLE completed\r\n").as_bytes())
                    .await
                    .expect("complete bounded IDLE");
                write.flush().await.expect("flush bounded IDLE completion");
                arrived = true;
                continue;
            } else if command == "LOGOUT" {
                assert!(
                    expected_metadata.is_empty(),
                    "all scheduled UIDs must be fetched"
                );
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write bounded logout");
                write.flush().await.expect("flush bounded logout");
                break;
            } else {
                panic!("unexpected bounded arrival command: {command}");
            };
            write
                .write_all(response.as_bytes())
                .await
                .expect("write bounded response");
            write.flush().await.expect("flush bounded response");
        }
        transcript
    });
    (endpoint, server)
}

async fn start_cancel_server(
    ready: oneshot::Sender<()>,
    initial_message: bool,
) -> (ImapEndpoint, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional cancellation listener");
    let endpoint = imap_test_endpoint(listener.local_addr().unwrap().port(), true);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept cancellation client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept cancellation TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let mut selects = 0_u8;
        let mut ready = Some(ready);
        let identity = if initial_message {
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 43] next\r\n"
        } else {
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 43] next\r\n"
        };
        write
            .write_all(b"* OK fictional public IMAP ready\r\n")
            .await
            .expect("write cancellation greeting");
        write.flush().await.expect("flush cancellation greeting");
        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1 IDLE\r\n{tag} OK capability\r\n")
            } else if command == "LOGIN \"reader@example.test\" \"realtime secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "SELECT INBOX" {
                selects += 1;
                format!("{identity}{tag} OK selected\r\n")
            } else if command == "FETCH 1:1 (UID FLAGS)" && initial_message {
                format!("* 1 FETCH (UID 42 FLAGS ())\r\n{tag} OK flags\r\n")
            } else if command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE") && initial_message {
                let headers = realtime_headers();
                format!(
                    "* 1 FETCH (UID 42 FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n{headers})\r\n{tag} OK metadata\r\n",
                    headers.len()
                )
            } else if command == "IDLE" {
                assert_eq!(selects, 3, "IDLE starts after a complete snapshot");
                write
                    .write_all(b"+ idling\r\n")
                    .await
                    .expect("write cancellation continuation");
                write
                    .flush()
                    .await
                    .expect("flush cancellation continuation");
                ready
                    .take()
                    .expect("IDLE continuation is signalled only once")
                    .send(())
                    .expect("test waits for IDLE continuation");
                let mut done = String::new();
                let count = read
                    .read_line(&mut done)
                    .await
                    .expect("read cancellation DONE");
                assert_ne!(count, 0);
                assert_eq!(done, "DONE\r\n");
                write
                    .write_all(format!("{tag} OK IDLE completed\r\n").as_bytes())
                    .await
                    .expect("complete cancelled IDLE");
                write.flush().await.expect("flush cancelled IDLE");
                continue;
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write cancellation logout");
                write.flush().await.expect("flush cancellation logout");
                break;
            } else {
                panic!("unexpected cancellation command: {command}");
            };
            write
                .write_all(response.as_bytes())
                .await
                .expect("write cancellation response");
            write.flush().await.expect("flush cancellation response");
        }
        transcript
    });
    (endpoint, server)
}

async fn realtime_fixture() -> (Store, MailService, Account) {
    let store = Store::in_memory()
        .await
        .expect("open fictional realtime store");
    let mut account = test_account();
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    store
        .save_account(&account)
        .await
        .expect("save realtime account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "realtime secret")
        .await
        .expect("save realtime credential");
    (store, service, account)
}

#[tokio::test]
async fn ms_b_014_public_tls_idle_disconnect_reconnect_arrival_flag_and_expunge() {
    let (store, service, account) = realtime_fixture().await;
    let (_sender, mut cancel) = watch::channel(false);

    let (endpoint, server) = start_realtime_server(RealtimeScript::Disconnect).await;
    let error = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect_err("IDLE disconnect must be retryable");
    assert!(
        error.to_string().contains("idle connection"),
        "disconnect remains a connection failure rather than a completed cycle: {error:#}"
    );
    assert!(server
        .await
        .expect("disconnect server")
        .iter()
        .any(|command| command == "IDLE"));
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read disconnected state")
            .is_none(),
        "a disconnected empty snapshot must not invent an arrival"
    );

    let (endpoint, server) = start_realtime_server(RealtimeScript::Arrival).await;
    let arrival = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("new public TLS session reconnects after IDLE loss");
    assert_eq!(arrival.mode, RealtimeMode::Idle);
    assert_eq!(arrival.new_messages.len(), 1);
    assert_eq!(arrival.new_messages[0].uid, 42);
    assert_eq!(arrival.new_messages[0].account_id, account.id.to_string());
    assert!(!arrival.catalogue_changed);
    let arrival_trace = server.await.expect("arrival server");
    assert!(arrival_trace.iter().any(|command| command == "IDLE"));
    assert!(
        arrival_trace
            .iter()
            .any(|command| command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE")),
        "reconnected cycle must fetch the exact arrived UID through the public connector"
    );

    let (endpoint, server) = start_realtime_server(RealtimeScript::FlagChange).await;
    let flags = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("flag invalidation reconciles through the public connector");
    assert!(flags.new_messages.is_empty());
    assert!(flags.catalogue_changed);
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read updated flag")
            .expect("UID 42 remains after flag update")
            .is_read,
        "IDLE flag change must reach the durable catalogue"
    );
    assert!(server
        .await
        .expect("flag server")
        .iter()
        .any(|command| command == "IDLE"));

    let (endpoint, server) = start_realtime_server(RealtimeScript::Expunge).await;
    let expunge = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("expunge invalidation reconciles through the public connector");
    assert!(expunge.new_messages.is_empty());
    assert!(expunge.catalogue_changed);
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read expunged state")
            .is_none(),
        "IDLE expunge must remove only the reconciled mailbox row"
    );
    assert!(server
        .await
        .expect("expunge server")
        .iter()
        .any(|command| command == "IDLE"));
}

#[tokio::test]
async fn ms_b_014_public_tls_seen_arrival_refreshes_without_notification() {
    let (store, service, account) = realtime_fixture().await;
    let (_sender, mut cancel) = watch::channel(false);

    // A completed, identity-matching empty snapshot is the explicit baseline
    // that permits a later UID to be treated as an arrival.
    let (endpoint, server) = start_realtime_server(RealtimeScript::Poll).await;
    service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("empty polling cycle establishes the baseline");
    server.await.expect("empty baseline server");

    let (endpoint, server) = start_realtime_server(RealtimeScript::SeenArrival).await;
    let seen = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("seen arrival reconciles through the public connector");
    assert!(seen.new_messages.is_empty(), "read mail must not notify");
    assert!(
        seen.catalogue_changed,
        "read mail still refreshes the catalogue"
    );
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read persisted Seen arrival")
            .expect("Seen arrival persists")
            .is_read
    );
    assert!(server
        .await
        .expect("Seen arrival server")
        .iter()
        .any(|command| command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE")));
}

#[tokio::test]
async fn ms_b_014_public_tls_empty_baseline_burst_advances_contiguously_after_reopen() {
    let directory = tempfile::tempdir().expect("create fictional profile");
    let database = directory.path().join("realtime-burst.db");
    let store = Store::open(&database)
        .await
        .expect("open fictional realtime profile");
    let mut account = test_account();
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    store.save_account(&account).await.expect("save account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "realtime secret")
        .await
        .expect("save credential");
    let (_sender, mut cancel) = watch::channel(false);

    let (endpoint, server) = start_realtime_server(RealtimeScript::Poll).await;
    service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("completed empty snapshot");
    server.await.expect("empty snapshot server");
    let baseline = store
        .prepare_mailbox_sync(account.id, "INBOX", Some(77))
        .await
        .expect("read empty baseline state");
    assert!(baseline.empty_baseline);

    drop(service);
    drop(store);
    let store = Store::open(&database)
        .await
        .expect("reopen fictional realtime profile");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "realtime secret")
        .await
        .expect("restore test credential");

    let (endpoint, server) = start_bounded_arrival_server(1..=50, true).await;
    let first = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("bounded first burst");
    assert_eq!(
        first
            .new_messages
            .iter()
            .map(|message| message.uid)
            .collect::<Vec<_>>(),
        (1_i64..=50).collect::<Vec<_>>(),
        "the first bounded burst exposes the oldest contiguous UIDs"
    );
    server.await.expect("first bounded server");
    assert_eq!(
        store
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("first watermark"),
        Some(50)
    );
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 1)
            .await
            .expect("read first UID")
            .is_some(),
        "the bounded cycle must not skip UID 1"
    );

    let (endpoint, server) = start_bounded_arrival_server([51], false).await;
    let retry = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("next watcher cycle converges the remaining UID");
    assert_eq!(
        retry
            .new_messages
            .iter()
            .map(|message| message.uid)
            .collect::<Vec<_>>(),
        [51]
    );
    server.await.expect("second bounded server");
    assert_eq!(
        store
            .highest_mailbox_uid(account.id, "INBOX")
            .await
            .expect("converged watermark"),
        Some(51)
    );
    assert_eq!(
        store
            .mailbox_uids(account.id, "INBOX")
            .await
            .expect("stored UIDs"),
        (1..=51).collect::<std::collections::HashSet<_>>()
    );
}

#[tokio::test]
async fn ms_b_014_public_tls_unknown_empty_watermark_catches_up_silently() {
    let (store, service, account) = realtime_fixture().await;
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 0, false)
        .await
        .expect("seed unknown historical catalogue");
    store
        .save_synced_messages(account.id, "INBOX", &[])
        .await
        .expect("seed unknown watermark");
    let state = store
        .prepare_mailbox_sync(account.id, "INBOX", Some(77))
        .await
        .expect("read unknown watermark");
    assert!(state.initialized);
    assert!(!state.empty_baseline);

    let (_cancel_sender, mut cancel) = watch::channel(false);
    let (ready_sender, _ready_receiver) = oneshot::channel();
    let (endpoint, server) = start_cancel_server(ready_sender, true).await;
    let cycle = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("unknown historical catch-up completes");
    assert!(cycle.new_messages.is_empty());
    assert!(cycle.catalogue_changed);
    assert!(!cycle.cancelled);
    assert!(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read catch-up message")
            .is_some(),
        "unknown history is persisted while no arrival is emitted"
    );
    assert!(server
        .await
        .expect("unknown watermark server")
        .iter()
        .all(|command| command != "IDLE"));
}

#[tokio::test]
async fn ms_b_014_public_tls_idle_renews_on_one_connection_with_production_policy() {
    let (_store, service, account) = realtime_fixture().await;
    let (_sender, mut cancel) = watch::channel(false);
    let (endpoint, server) = start_realtime_server(RealtimeScript::Renewal).await;
    let schedule = RealtimeSchedule {
        reconcile_interval: Duration::from_millis(20),
        maintenance_interval: Duration::from_millis(65),
        noop_invalidation_min_interval: Duration::from_millis(1),
    };

    let renewed = tokio::time::timeout(
        Duration::from_secs(3),
        service.realtime_inbox_cycle_with_imap_endpoint_and_schedule_for_test(
            &account,
            50,
            &mut cancel,
            endpoint,
            schedule,
            || {},
        ),
    )
    .await
    .expect("the production renewal deadline must end the first IDLE session")
    .expect("a clean IDLE renewal remains a successful realtime cycle");
    assert_eq!(renewed.mode, RealtimeMode::Idle);
    assert!(!renewed.cancelled);
    assert!(renewed.new_messages.is_empty());
    assert_eq!(IMAP_IDLE_RECONCILE_INTERVAL, Duration::from_secs(60));
    assert_eq!(
        IMAP_REALTIME_MAINTENANCE_INTERVAL,
        Duration::from_secs(15 * 60)
    );
    assert!(IMAP_REALTIME_MAINTENANCE_INTERVAL < Duration::from_secs(29 * 60));
    let renewal_trace = server.await.expect("renewal server");
    let idle_count = renewal_trace
        .iter()
        .filter(|command| command.as_str() == "IDLE")
        .count();
    let done_count = renewal_trace
        .iter()
        .filter(|command| command.as_str() == "DONE")
        .count();
    assert!(
        idle_count >= 2,
        "one TLS/auth session must serve multiple polls"
    );
    assert_eq!(done_count, idle_count);
    assert_eq!(
        renewal_trace
            .iter()
            .filter(|command| command.starts_with("LOGIN "))
            .count(),
        1,
        "quiet polls must not create an authentication storm"
    );
    let done = renewal_trace
        .iter()
        .position(|command| command == "DONE")
        .expect("renewal transcript includes DONE");
    let logout = renewal_trace
        .iter()
        .position(|command| command == "LOGOUT")
        .expect("renewal transcript includes LOGOUT");
    assert!(done < logout);

    // The maintenance heartbeat returns control after several clean renewals.
    // Exercise the next authenticated cycle separately and prove that it
    // reaches a fresh IDLE subscription before cancellation. The desktop
    // watcher tests prove that a successful cycle immediately starts the next.
    let (cancel_sender, mut cancel) = watch::channel(false);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (endpoint, server) = start_cancel_server(ready_sender, false).await;
    let resubscribed = tokio::spawn({
        let service = service.clone();
        let account = account.clone();
        async move {
            service
                .realtime_inbox_cycle_with_imap_endpoint_for_test(
                    &account,
                    50,
                    &mut cancel,
                    endpoint,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), ready_receiver)
        .await
        .expect("the next TLS session must resubscribe promptly")
        .expect("server observes the next IDLE continuation");
    cancel_sender
        .send(true)
        .expect("cancel renewed subscription");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), resubscribed)
            .await
            .expect("renewed subscription cancellation completes")
            .expect("renewed subscription task")
            .expect("renewed subscription remains healthy")
            .cancelled
    );
    let resubscribe_trace = server.await.expect("resubscribe server");
    assert!(resubscribe_trace.iter().any(|command| command == "IDLE"));
    assert!(resubscribe_trace.iter().any(|command| command == "LOGOUT"));
}

#[tokio::test]
async fn ms_b_014_public_tls_noop_idle_noise_is_paced_on_one_connection() {
    let (_store, service, account) = realtime_fixture().await;
    let (_sender, mut cancel) = watch::channel(false);
    let (endpoint, server) = start_realtime_server(RealtimeScript::NoopNoise).await;
    let pace = Duration::from_millis(10);
    let schedule = RealtimeSchedule {
        reconcile_interval: Duration::from_secs(1),
        maintenance_interval: Duration::from_millis(45),
        noop_invalidation_min_interval: pace,
    };
    let started = Instant::now();

    let cycle = tokio::time::timeout(
        Duration::from_secs(3),
        service.realtime_inbox_cycle_with_imap_endpoint_and_schedule_for_test(
            &account,
            50,
            &mut cancel,
            endpoint,
            schedule,
            || {},
        ),
    )
    .await
    .expect("bounded no-op noise cycle must finish")
    .expect("no-op invalidations remain a healthy realtime cycle");
    assert!(cycle.new_messages.is_empty());
    assert!(!cycle.catalogue_changed);
    assert!(!cycle.cancelled);

    let trace = server.await.expect("no-op noise server");
    let idle_count = trace
        .iter()
        .filter(|command| command.as_str() == "IDLE")
        .count();
    assert!(
        idle_count >= 2,
        "noise must exercise repeated reconciliation"
    );
    assert!(
        idle_count <= 6,
        "minimum pacing must bound immediate invalidation churn: {idle_count} IDLE commands"
    );
    assert!(
        started.elapsed() >= pace.saturating_mul((idle_count - 1) as u32),
        "each repeated no-op invalidation must pay the configured cancellable pace"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|command| command.starts_with("LOGIN "))
            .count(),
        1,
        "no-op noise must not create an authentication storm"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|command| command.as_str() == "DONE")
            .count(),
        idle_count
    );
}

#[tokio::test]
async fn ms_l_010_public_tls_idle_cancellation_and_polling_fallback_release_the_session() {
    // The very first nonempty inventory establishes the initial baseline. It
    // must persist the message but remain in IDLE instead of publishing a
    // historical arrival notification.
    let (initial_store, service, account) = realtime_fixture().await;
    let (_initial_cancel_sender, mut initial_cancel) = watch::channel(false);
    let (initial_ready_sender, _initial_ready_receiver) = oneshot::channel();
    let (endpoint, initial_server) = start_cancel_server(initial_ready_sender, true).await;
    let initial_cycle = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(
            &account,
            50,
            &mut initial_cancel,
            endpoint,
        )
        .await
        .expect("initial baseline completes");
    assert!(initial_cycle.catalogue_changed);
    assert!(!initial_cycle.cancelled);
    assert!(
        initial_store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .expect("read persisted initial baseline")
            .is_some(),
        "the silent first baseline still persists its real message"
    );
    let initial_trace = initial_server.await.expect("initial baseline server");
    assert!(initial_trace.iter().all(|command| command != "IDLE"));
    assert!(initial_trace
        .iter()
        .any(|command| command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE")));

    let (_store, service, account) = realtime_fixture().await;
    let (cancel_sender, mut cancel) = watch::channel(false);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (endpoint, server) = start_cancel_server(ready_sender, false).await;
    let cycle = tokio::spawn({
        let service = service.clone();
        let account = account.clone();
        async move {
            service
                .realtime_inbox_cycle_with_imap_endpoint_for_test(
                    &account,
                    50,
                    &mut cancel,
                    endpoint,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), ready_receiver)
        .await
        .expect("server must reach IDLE promptly")
        .expect("server must signal IDLE continuation");
    cancel_sender.send(true).expect("cancel active cycle");
    let cancelled = tokio::time::timeout(Duration::from_secs(2), cycle)
        .await
        .expect("cancellation must release the public TLS session")
        .expect("cancellation task")
        .expect("cancelled IDLE is not an error");
    assert!(cancelled.cancelled);
    let cancel_trace = server.await.expect("cancellation server");
    assert!(cancel_trace.iter().any(|command| command == "IDLE"));
    assert!(cancel_trace.iter().any(|command| command == "LOGOUT"));

    let (_store, service, account) = realtime_fixture().await;
    let (_sender, mut cancel) = watch::channel(false);
    let (endpoint, server) = start_realtime_server(RealtimeScript::Poll).await;
    let polling = service
        .realtime_inbox_cycle_with_imap_endpoint_for_test(&account, 50, &mut cancel, endpoint)
        .await
        .expect("missing IDLE capability falls back to a completed polling cycle");
    assert_eq!(polling.mode, RealtimeMode::Poll);
    assert!(!polling.catalogue_changed);
    let poll_trace = server.await.expect("polling server");
    assert!(poll_trace.iter().all(|command| command != "IDLE"));
    assert!(poll_trace.iter().any(|command| command == "LOGOUT"));
}
