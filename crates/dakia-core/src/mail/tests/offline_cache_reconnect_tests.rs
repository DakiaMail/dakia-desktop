use super::*;
use tokio::{
    io::{split, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

const ACCOUNT_ID: &str = "11111111-1111-4111-8111-111111111111";
const SECOND_ACCOUNT_ID: &str = "22222222-2222-4222-8222-222222222222";
const MAILBOX_UID_VALIDITY: i64 = 77;

fn offline_account() -> Account {
    let mut account = imap_test_account(Security::Tls, 993);
    account.id = uuid::Uuid::parse_str(ACCOUNT_ID).expect("fixed fictional account id");
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    account
}

fn second_offline_account() -> Account {
    let mut account = offline_account();
    account.id = uuid::Uuid::parse_str(SECOND_ACCOUNT_ID).expect("fixed second account id");
    account.email = "second@example.test".into();
    account.account_name = "Second fictional inbox".into();
    account.auth = AccountAuth::Password {
        username: "second@example.test".into(),
    };
    account
}

async fn seed_catalogue(store: &Store, account: &Account, uid: u32) -> MailSummary {
    seed_catalogue_with_message_id(
        store,
        account,
        uid,
        &format!("<offline-{uid}@example.test>"),
    )
    .await
}

async fn seed_catalogue_with_message_id(
    store: &Store,
    account: &Account,
    uid: u32,
    message_id: &str,
) -> MailSummary {
    store
        .save_mailbox_catalog_state(
            account.id,
            "INBOX",
            "INBOX",
            MAILBOX_UID_VALIDITY as u32,
            2,
            true,
        )
        .await
        .expect("save fictional mailbox state");
    let headers = format!(
        "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <{}>\r\nSubject: Fictional offline message {uid}\r\nMessage-ID: {message_id}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
        account.email
    );
    let mut message = parse_catalog_message(
        account,
        "INBOX",
        uid,
        &[],
        headers.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue fixture parses");
    message.mailbox_uid_validity = Some(MAILBOX_UID_VALIDITY);
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("save fictional catalogue row");
    message
}

async fn scripted_selective_fetch_server(
    body: impl Into<String>,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    scripted_selective_fetch_server_for(
        body,
        "reader@example.test",
        "offline secret",
        "reader@example.test",
        "<offline-42@example.test>",
    )
    .await
}

async fn scripted_selective_fetch_server_for(
    body: impl Into<String>,
    username: impl Into<String>,
    password: impl Into<String>,
    recipient: impl Into<String>,
    message_id: impl Into<String>,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let body = body.into();
    let username = username.into();
    let password = password.into();
    let recipient = recipient.into();
    let message_id = message_id.into();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept test TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <{recipient}>\r\nSubject: Fictional cached reader message\r\nMessage-ID: {message_id}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
        );
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n")
            } else if command
                == format!("LOGIN {} {}", quote_imap(&username), quote_imap(&password))
            {
                format!("{tag} OK authenticated\r\n")
            } else if command == "SELECT \"INBOX\"" {
                format!(
                    "* 2 EXISTS\r\n* OK [UIDVALIDITY {MAILBOX_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                )
            } else if command == "UID FETCH 42 (BODY.PEEK[TEXT])" {
                format!(
                    "* 1 FETCH (UID 42 BODY[TEXT] {{{}}}\r\n{}\r\n)\r\n{tag} OK body\r\n",
                    body.len(),
                    body
                )
            } else if command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE BODYSTRUCTURE") {
                format!(
                    "* 1 FETCH (UID 42 FLAGS () INTERNALDATE \"28-Jul-2026 12:00:00 +0000\" BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" {} 1) BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n{}\r\n)\r\n{tag} OK metadata\r\n",
                    body.len(), headers.len(), headers
                )
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional selective-fetch command: {command}");
            };
            write
                .write_all(response.as_bytes())
                .await
                .expect("write fictional IMAP response");
            write.flush().await.expect("flush fictional IMAP response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

async fn unused_loopback_endpoint() -> ImapEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve an unavailable loopback port");
    let port = listener.local_addr().expect("reserved address").port();
    drop(listener);
    imap_test_endpoint(port, true)
}

#[tokio::test]
async fn offline_cache_survives_reopen_uncached_failure_and_same_epoch_reconnect() {
    let directory = tempfile::tempdir().expect("temporary fictional profile");
    let database = directory.path().join("offline-cache.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let account = offline_account();
    store
        .save_account(&account)
        .await
        .expect("save fictional account");
    let cached_target = seed_catalogue(&store, &account, 42).await;
    let uncached_target = seed_catalogue(&store, &account, 43).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "offline secret")
        .await
        .expect("save fictional credential");

    let (fetch_endpoint, fetch_server) =
        scripted_selective_fetch_server("Fetched body before persistence.").await;
    let fetched = service
        .fetch_message_at_epoch_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            fetch_endpoint,
        )
        .await
        .expect("fetch fictional reader message through the production selective path");
    assert_eq!(fetched.id, cached_target.id);
    assert_eq!(fetched.account_id, account.id.to_string());
    assert_eq!(fetched.mailbox_uid_validity, Some(MAILBOX_UID_VALIDITY));
    assert_eq!(fetched.body_text, "Fetched body before persistence.");
    fetch_server.await.expect("fetch server task");

    let (initial_endpoint, initial_server) =
        scripted_selective_fetch_server("Cached body before reconnect.").await;
    let initial = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            initial_endpoint,
        )
        .await
        .expect("hydrate fictional cached message");
    assert_eq!(initial.id, cached_target.id);
    assert_eq!(initial.account_id, account.id.to_string());
    assert_eq!(initial.mailbox_uid_validity, Some(MAILBOX_UID_VALIDITY));
    assert_eq!(initial.content_state, "complete");
    assert_eq!(
        initial_server.await.expect("initial server task"),
        vec![
            "CAPABILITY".into(),
            "LOGIN \"reader@example.test\" \"offline secret\"".into(),
            "SELECT \"INBOX\"".into(),
            initial_fetch_metadata_command(),
            "UID FETCH 42 (BODY.PEEK[TEXT])".into(),
            "LOGOUT".into(),
        ],
        "the cached reader body uses the production selective-fetch sequence"
    );

    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile offline");
    let cached = reopened
        .cached_message_content(&cached_target.id)
        .await
        .expect("read offline cache")
        .expect("hydrated body remains available offline");
    assert_eq!(cached.body_text, "Cached body before reconnect.");
    assert!(
        reopened
            .cached_message_content(&uncached_target.id)
            .await
            .expect("read uncached sibling")
            .is_none(),
        "an unopened sibling must not masquerade as cached mail"
    );

    let reconnected = MailService::new(reopened.clone());
    let offline_error = reconnected
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            43,
            MAILBOX_UID_VALIDITY,
            unused_loopback_endpoint().await,
        )
        .await
        .expect_err("uncached mail cannot be read without a provider connection");
    assert!(
        format!("{offline_error:#}").contains("could not connect to IMAP server"),
        "the uncached failure must identify the unavailable mail connection"
    );
    assert_eq!(
        reopened
            .cached_message_content(&cached_target.id)
            .await
            .expect("read cached sibling after failure")
            .expect("cached sibling survives a failed uncached fetch")
            .body_text,
        "Cached body before reconnect."
    );

    let (reconnect_endpoint, reconnect_server) =
        scripted_selective_fetch_server("Fresh body after reconnect.").await;
    let refreshed = reconnected
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            reconnect_endpoint,
        )
        .await
        .expect("reconnect refreshes the same fictional mailbox epoch");
    assert_eq!(refreshed.id, cached_target.id);
    assert_eq!(refreshed.account_id, account.id.to_string());
    assert_eq!(refreshed.mailbox_uid_validity, Some(MAILBOX_UID_VALIDITY));
    assert_eq!(refreshed.content_state, "complete");
    reconnect_server.await.expect("reconnect server task");
    assert_eq!(
        reopened
            .cached_message_content(&cached_target.id)
            .await
            .expect("read refreshed cache")
            .expect("refreshed body remains cached")
            .body_text,
        "Fresh body after reconnect."
    );
    assert_eq!(
        reopened
            .message(&cached_target.id)
            .await
            .expect("read refreshed message")
            .expect("same message row remains")
            .mailbox_uid_validity,
        Some(MAILBOX_UID_VALIDITY),
        "reconnect must refresh the captured account and epoch, not a different mailbox row"
    );
}

#[tokio::test]
async fn gmail_reader_without_exact_extension_uses_generic_metadata_and_preserves_private_identity()
{
    let store = Store::in_memory().await.expect("open fictional profile");
    let mut account = offline_account();
    account.provider_id = "gmail".into();
    store
        .save_account(&account)
        .await
        .expect("save Gmail account");
    let mut known = seed_catalogue(&store, &account, 42).await;
    known.gmail_message_id = Some("9201".into());
    store
        .upsert_catalog_messages(std::slice::from_ref(&known))
        .await
        .expect("seed known provider identity");
    let target = store
        .message_by_locator(account.id, "INBOX", 42)
        .await
        .expect("load target")
        .expect("target exists");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "offline secret")
        .await
        .expect("save fictional credential");

    let (endpoint, server) = scripted_selective_fetch_server("Gmail without extension.").await;
    let fetched = service
        .fetch_message_at_epoch_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            endpoint,
        )
        .await
        .expect("fetch through the production reader path");
    assert_eq!(fetched.gmail_message_id, None);
    assert!(store
        .publish_message_content_at_epoch(&target, &fetched)
        .await
        .expect("publish reader content"));
    let transcript = server.await.expect("reader server task");
    assert_eq!(transcript[0], "CAPABILITY");
    assert_eq!(
        transcript[1],
        "LOGIN \"reader@example.test\" \"offline secret\""
    );
    assert_eq!(transcript[2], "CAPABILITY");
    assert!(transcript
        .iter()
        .all(|command| !command.contains("X-GM-MSGID") && !command.contains("X-GM-LABELS")));
    assert_eq!(
        store
            .gmail_provider_identity_for_message(&target.id)
            .await
            .expect("read known identity"),
        Some("9201".into())
    );
}

#[tokio::test]
async fn offline_cache_and_reconnect_keep_colliding_mail_identity_in_its_account() {
    const SHARED_MESSAGE_ID: &str = "<shared-offline-message@example.test>";
    const SECOND_SECRET: &str = "second offline secret";

    let directory = tempfile::tempdir().expect("temporary two-account profile");
    let database = directory.path().join("two-account-offline-cache.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open two-account fictional profile");
    let first_account = offline_account();
    let second_account = second_offline_account();
    for account in [&first_account, &second_account] {
        store
            .save_account(account)
            .await
            .expect("save fictional account");
    }
    let first_target =
        seed_catalogue_with_message_id(&store, &first_account, 42, SHARED_MESSAGE_ID).await;
    let second_target =
        seed_catalogue_with_message_id(&store, &second_account, 42, SHARED_MESSAGE_ID).await;
    assert_ne!(
        first_target.id, second_target.id,
        "stable local identity must include the account"
    );
    assert_eq!(first_target.uid, second_target.uid);
    assert_eq!(first_target.message_id, second_target.message_id);
    assert_eq!(first_target.thread_id, second_target.thread_id);
    let first_persisted = store
        .message(&first_target.id)
        .await
        .expect("read first colliding row")
        .expect("first colliding row exists");
    let second_persisted = store
        .message(&second_target.id)
        .await
        .expect("read second colliding row")
        .expect("second colliding row exists");
    assert_eq!(first_persisted.message_id, second_persisted.message_id);
    assert_eq!(first_persisted.thread_id, second_persisted.thread_id);

    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&first_account, "first offline secret")
        .await
        .expect("save first fictional credential");
    service
        .credentials()
        .set_password(&second_account, SECOND_SECRET)
        .await
        .expect("save second fictional credential");

    let (initial_endpoint, initial_server) = scripted_selective_fetch_server_for(
        "Second account cached body.",
        second_account.auth.username(),
        SECOND_SECRET,
        &second_account.email,
        SHARED_MESSAGE_ID,
    )
    .await;
    let initial = service
        .hydrate_message_with_imap_endpoint_for_test(
            &second_account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            initial_endpoint,
        )
        .await
        .expect("hydrate only the second account through production parsing");
    assert_eq!(initial.id, second_target.id);
    assert_eq!(initial.account_id, second_account.id.to_string());
    assert_eq!(initial.message_id, second_target.message_id);
    assert_eq!(initial.thread_id, second_persisted.thread_id);
    initial_server.await.expect("initial second-account server");

    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen two-account profile offline");
    assert_eq!(
        reopened
            .cached_message_content(&second_target.id)
            .await
            .expect("read second-account cache")
            .expect("second-account content remains cached")
            .body_text,
        "Second account cached body."
    );
    assert!(
        reopened
            .cached_message_content(&first_target.id)
            .await
            .expect("read first-account cache state")
            .is_none(),
        "the colliding first-account message must remain uncached"
    );

    let reconnected = MailService::new(reopened.clone());
    let first_offline_error = reconnected
        .hydrate_message_with_imap_endpoint_for_test(
            &first_account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            unused_loopback_endpoint().await,
        )
        .await
        .expect_err("the uncached first account cannot borrow the second account cache");
    assert!(
        format!("{first_offline_error:#}").contains("could not connect to IMAP server"),
        "the uncached account must report the unavailable provider connection"
    );
    assert!(reopened
        .cached_message_content(&first_target.id)
        .await
        .expect("recheck first-account cache state")
        .is_none());
    assert_eq!(
        reopened
            .cached_message_content(&second_target.id)
            .await
            .expect("recheck second-account cache")
            .expect("failed first-account fetch cannot evict second-account content")
            .body_text,
        "Second account cached body."
    );

    let (reconnect_endpoint, reconnect_server) = scripted_selective_fetch_server_for(
        "Second account refreshed body.",
        second_account.auth.username(),
        SECOND_SECRET,
        &second_account.email,
        SHARED_MESSAGE_ID,
    )
    .await;
    let refreshed = reconnected
        .hydrate_message_with_imap_endpoint_for_test(
            &second_account,
            "INBOX",
            42,
            MAILBOX_UID_VALIDITY,
            reconnect_endpoint,
        )
        .await
        .expect("reconnect the exact second-account mailbox epoch");
    reconnect_server
        .await
        .expect("reconnect second-account server");
    assert_eq!(refreshed.id, second_target.id);
    assert_eq!(refreshed.account_id, second_account.id.to_string());
    assert_eq!(refreshed.mailbox_uid_validity, Some(MAILBOX_UID_VALIDITY));
    assert_eq!(refreshed.message_id, second_target.message_id);
    assert_eq!(refreshed.thread_id, second_persisted.thread_id);
    assert_eq!(
        reopened
            .cached_message_content(&second_target.id)
            .await
            .expect("read refreshed second-account cache")
            .expect("refreshed second-account content remains cached")
            .body_text,
        "Second account refreshed body."
    );
    assert!(
        reopened
            .cached_message_content(&first_target.id)
            .await
            .expect("final first-account cache state")
            .is_none(),
        "second-account reconnect must not populate the colliding first-account row"
    );
}

fn initial_fetch_metadata_command() -> String {
    "UID FETCH 42 (FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)])".into()
}
