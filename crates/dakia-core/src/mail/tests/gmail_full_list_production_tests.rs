//! Explicit, manually authorized Gmail full-LIST evidence.
//!
//! This ignored test uses the production TLS endpoint and only sends IMAP
//! capability discovery, password authentication, `LIST "" "*"`, and `LOGOUT`.
//! It never selects a mailbox, fetches message data, or changes remote state.

use super::*;
use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use std::future::pending;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    net::TcpListener,
    sync::oneshot,
    time::{timeout, Duration},
};

const GMAIL_LIST_PHASE_TIMEOUT: Duration = Duration::from_secs(20);
const GMAIL_LIST_GLOBAL_TIMEOUT: Duration = Duration::from_secs(45);
const GMAIL_LIST_FAILURE: &str = "Gmail full-LIST probe did not complete";
const GMAIL_LIST_CREDENTIALS_REQUIRED: &str =
    "Gmail full-LIST probe requires authorized disposable credentials";

#[derive(Clone, Copy)]
struct ProbeTimeouts {
    connect: Duration,
    phase: Duration,
    total: Duration,
}

const PRODUCTION_TIMEOUTS: ProbeTimeouts = ProbeTimeouts {
    connect: GMAIL_LIST_PHASE_TIMEOUT,
    phase: GMAIL_LIST_PHASE_TIMEOUT,
    total: GMAIL_LIST_GLOBAL_TIMEOUT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum LogoutOutcome {
    /// The production parser observed the tagged completion after the normal BYE.
    TaggedOk,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct GmailListObservation {
    listed_count: usize,
    selectable_count: usize,
    noselect_count: usize,
    inbox_present: bool,
    listing_shape_sha256: String,
    logout_outcome: LogoutOutcome,
}

fn disposable_gmail_account(email: String) -> Account {
    Account {
        id: uuid::Uuid::parse_str("4d53d9e2-4ed4-4b20-ae82-0e2bfd2e278f")
            .expect("fixed disposable Gmail account UUID is valid"),
        account_name: "Disposable Gmail full-LIST probe".into(),
        display_name: "Dakia Provider Test".into(),
        auth: AccountAuth::Password {
            username: email.clone(),
        },
        email,
        provider_id: "gmail".into(),
        imap_host: "imap.gmail.com".into(),
        imap_port: 993,
        imap_security: Security::Tls,
        // SMTP is deliberately inert. This test never creates an SMTP client.
        smtp_host: "smtp.invalid".into(),
        smtp_port: 0,
        smtp_security: Security::Tls,
        archive_mailbox: "[Gmail]/All Mail".into(),
        spam_mailbox: "[Gmail]/Spam".into(),
        enabled: true,
        created_at: chrono::Utc::now(),
    }
}

fn required_disposable_credential(name: &str) -> Result<String> {
    let value = std::env::var(name).map_err(|_| anyhow!(GMAIL_LIST_CREDENTIALS_REQUIRED))?;
    if value.trim().is_empty() {
        return Err(anyhow!(GMAIL_LIST_CREDENTIALS_REQUIRED));
    }
    Ok(value)
}

fn observe_listing(listing: &[ListedMailbox]) -> Result<GmailListObservation> {
    if listing.is_empty() {
        return Err(anyhow!(GMAIL_LIST_FAILURE));
    }
    let inboxes = listing
        .iter()
        .filter(|mailbox| same_mailbox_wire_identity(&mailbox.remote_name, "INBOX"))
        .collect::<Vec<_>>();
    if inboxes.len() != 1 || !inboxes[0].is_selectable() {
        return Err(anyhow!(GMAIL_LIST_FAILURE));
    }
    let selectable_count = listing
        .iter()
        .filter(|mailbox| mailbox.is_selectable())
        .count();
    let mut shape = listing
        .iter()
        .map(|mailbox| {
            let mut flags = mailbox.flags.clone();
            flags.sort_unstable();
            format!(
                "{}:{}:{}",
                usize::from(mailbox.is_selectable()),
                usize::from(mailbox.hierarchy_delimiter.is_some()),
                flags.join("\\u{1f}"),
            )
        })
        .collect::<Vec<_>>();
    shape.sort_unstable();
    Ok(GmailListObservation {
        listed_count: listing.len(),
        selectable_count,
        noselect_count: listing.len().saturating_sub(selectable_count),
        inbox_present: true,
        listing_shape_sha256: format!("{:x}", Sha256::digest(shape.join("\\n").as_bytes())),
        logout_outcome: LogoutOutcome::TaggedOk,
    })
}

async fn assert_eof_without_followup<S>(connection: &mut BufReader<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut command = String::new();
    match timeout(
        Duration::from_millis(100),
        connection.read_line(&mut command),
    )
    .await
    .expect("invalid LIST client must close without a follow-up command")
    {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Ok(_) => panic!("invalid LIST must not issue a follow-up command"),
        Err(error) => panic!("invalid LIST socket read failed: {error}"),
    }
}

async fn close_after_list<S>(
    client: &mut ImapClient<S>,
    phase_timeout: Duration,
) -> Result<LogoutOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(phase_timeout, client.command("LOGOUT"))
        .await
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?;
    Ok(LogoutOutcome::TaggedOk)
}

async fn authenticated_full_list_read_neutral<S>(
    client: &mut ImapClient<S>,
    account: &Account,
    password: &str,
    timeouts: ProbeTimeouts,
) -> Result<GmailListObservation>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(timeouts.phase, client.authenticate(account, password))
        .await
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?;
    let listing = timeout(timeouts.phase, client.list_mailboxes())
        .await
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?;
    let observation = observe_listing(&listing)?;
    close_after_list(client, timeouts.phase).await?;
    Ok(observation)
}

async fn full_list_with_endpoint(
    account: &Account,
    password: &str,
    endpoint: ImapEndpoint,
    timeouts: ProbeTimeouts,
) -> Result<GmailListObservation> {
    timeout(timeouts.total, async {
        let mut client = ImapClient::connect_with_endpoint_and_timeouts(
            account,
            endpoint,
            ImapConnectionTimeouts {
                connect: timeouts.connect,
                command: timeouts.connect,
            },
        )
        .await
        .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?;
        authenticated_full_list_read_neutral(&mut client, account, password, timeouts).await
    })
    .await
    .map_err(|_| anyhow!(GMAIL_LIST_FAILURE))?
}

async fn production_gmail_full_list() -> Result<GmailListObservation> {
    let account = disposable_gmail_account(required_disposable_credential("GMAIL_ACCOUNT")?);
    let password = required_disposable_credential("GMAIL_PASSWORD")?;
    full_list_with_endpoint(
        &account,
        &password,
        ImapEndpoint::production(&account),
        PRODUCTION_TIMEOUTS,
    )
    .await
}

#[tokio::test]
#[ignore = "requires explicit disposable Gmail credentials and a read-only provider authorization"]
async fn production_gmail_full_list_is_read_neutral() -> Result<()> {
    let observation = production_gmail_full_list().await?;
    // This one fixed-schema line excludes mailbox names, account identity,
    // credentials, message content, and server error text.
    println!(
        "DAKIA_GMAIL_FULL_LIST_EVIDENCE:{}",
        serde_json::to_string(&observation).expect("fixed listing evidence serializes")
    );
    Ok(())
}

#[tokio::test]
async fn scripted_tls_full_list_allows_only_authentication_list_and_tagged_logout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        for (expected, response) in [
            ("D0001 CAPABILITY\r\n", "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n"),
            ("D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n", "D0002 OK authenticated\r\n"),
            ("D0003 LIST \"\" \"*\"\r\n", "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Noselect) \"/\" \"fictional-parent\"\r\nD0003 OK listed\r\n"),
            ("D0004 LOGOUT\r\n", "* BYE done\r\nD0004 OK logout\r\n"),
        ] {
            assert_eq!(smtp_command(&mut connection).await, expected);
            smtp_reply(&mut connection, response).await;
        }
    });

    let observation = full_list_with_endpoint(
        &account,
        "fictional app password",
        imap_test_endpoint(account.imap_port, true),
        ProbeTimeouts {
            connect: Duration::from_secs(2),
            phase: Duration::from_secs(2),
            total: Duration::from_secs(4),
        },
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(observation.listed_count, 2);
    assert_eq!(observation.selectable_count, 1);
    assert_eq!(observation.noselect_count, 1);
    assert!(observation.inbox_present);
    assert_eq!(observation.logout_outcome, LogoutOutcome::TaggedOk);
    assert_eq!(observation.listing_shape_sha256.len(), 64);
    assert_ne!(observation.listing_shape_sha256, "INBOX");
}

#[tokio::test]
async fn stalled_authentication_is_bounded_and_does_not_issue_list() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n"
        );
        pending::<()>().await;
    });
    let error = full_list_with_endpoint(
        &account,
        "fictional app password",
        imap_test_endpoint(account.imap_port, true),
        ProbeTimeouts {
            connect: Duration::from_millis(100),
            phase: Duration::from_millis(25),
            total: Duration::from_millis(200),
        },
    )
    .await
    .unwrap_err();
    server.abort();
    assert_eq!(error.to_string(), GMAIL_LIST_FAILURE);
}

#[tokio::test]
async fn stalled_list_is_bounded_and_does_not_issue_logout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n"
        );
        smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        pending::<()>().await;
    });
    let error = full_list_with_endpoint(
        &account,
        "fictional app password",
        imap_test_endpoint(account.imap_port, true),
        ProbeTimeouts {
            connect: Duration::from_millis(100),
            phase: Duration::from_millis(25),
            total: Duration::from_millis(200),
        },
    )
    .await
    .unwrap_err();
    server.abort();
    assert_eq!(error.to_string(), GMAIL_LIST_FAILURE);
}

#[tokio::test]
async fn stalled_logout_is_bounded_by_the_phase_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let (logout_received, logout_observed) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n"
        );
        smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0003 OK listed\r\n",
        )
        .await;
        assert_eq!(smtp_command(&mut connection).await, "D0004 LOGOUT\r\n");
        logout_received.send(()).unwrap();
        pending::<()>().await;
    });
    let probe_account = account.clone();
    let probe = tokio::spawn(async move {
        full_list_with_endpoint(
            &probe_account,
            "fictional app password",
            imap_test_endpoint(probe_account.imap_port, true),
            ProbeTimeouts {
                connect: Duration::from_secs(5),
                phase: Duration::from_millis(250),
                total: Duration::from_secs(30),
            },
        )
        .await
    });
    timeout(Duration::from_secs(5), logout_observed)
        .await
        .expect("server must observe LOGOUT")
        .expect("probe must retain its LOGOUT receiver");
    let result = timeout(Duration::from_secs(2), probe)
        .await
        .expect("LOGOUT must obey the shorter phase deadline")
        .unwrap()
        .unwrap_err();
    server.abort();
    assert_eq!(result.to_string(), GMAIL_LIST_FAILURE);
}

#[tokio::test]
async fn missing_inbox_list_is_rejected_before_logout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n"
        );
        smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"Projects\"\r\nD0003 OK listed\r\n",
        )
        .await;
        assert_eof_without_followup(&mut connection).await;
    });
    let error = full_list_with_endpoint(
        &account,
        "fictional app password",
        imap_test_endpoint(account.imap_port, true),
        ProbeTimeouts {
            connect: Duration::from_secs(2),
            phase: Duration::from_secs(2),
            total: Duration::from_secs(4),
        },
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    assert_eq!(error.to_string(), GMAIL_LIST_FAILURE);
}

#[tokio::test]
async fn malformed_list_is_rejected_before_logout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional Gmail ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0002 LOGIN \"reader@example.test\" \"fictional app password\"\r\n"
        );
        smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\"\r\nD0003 OK listed\r\n",
        )
        .await;
        assert_eof_without_followup(&mut connection).await;
    });
    let error = full_list_with_endpoint(
        &account,
        "fictional app password",
        imap_test_endpoint(account.imap_port, true),
        ProbeTimeouts {
            connect: Duration::from_secs(2),
            phase: Duration::from_secs(2),
            total: Duration::from_secs(4),
        },
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    assert_eq!(error.to_string(), GMAIL_LIST_FAILURE);
}

#[test]
fn credential_requirements_use_fixed_errors_without_echoing_values() {
    let error = required_disposable_credential("DAKIA_MISSING_GMAIL_LIST_TEST_VALUE")
        .expect_err("missing private credential must fail closed");
    assert_eq!(error.to_string(), GMAIL_LIST_CREDENTIALS_REQUIRED);
    assert!(!error.to_string().contains("GMAIL_PASSWORD"));
}

#[test]
fn observation_hash_never_serializes_mailbox_names() {
    let observation = observe_listing(&[
        ListedMailbox {
            remote_name: "INBOX".into(),
            flags: vec!["\\HasNoChildren".into()],
            hierarchy_delimiter: Some("/".into()),
        },
        ListedMailbox {
            remote_name: "sensitive-folder".into(),
            flags: vec!["\\HasNoChildren".into()],
            hierarchy_delimiter: Some("/".into()),
        },
    ])
    .unwrap();
    let encoded = serde_json::to_string(&observation).unwrap();
    assert!(!encoded.contains("sensitive-folder"));
}
