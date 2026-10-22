//! Manual integration against an independently implemented, loopback IMAP
//! server. The launcher supplies a one-run CA through the environment; this
//! module never accepts a system or public credential.

use crate::{Account, AccountAuth, ComposeMessage, MailService, Security, Store};
use chrono::Utc;
use std::{env, fs};
use uuid::Uuid;

const HOST: &str = "DAKIA_INDEPENDENT_SERVER_HOST";
const PORT: &str = "DAKIA_INDEPENDENT_SERVER_PORT";
const SERVER_NAME: &str = "DAKIA_INDEPENDENT_SERVER_NAME";
const CA_DER_PATH: &str = "DAKIA_INDEPENDENT_CA_DER_PATH";
const USERNAME: &str = "DAKIA_INDEPENDENT_USERNAME";
const PASSWORD: &str = "DAKIA_INDEPENDENT_PASSWORD";
const SMTP_PORT: &str = "DAKIA_INDEPENDENT_SMTP_PORT";

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} is required for the independent server test"))
}

fn loopback_host(value: &str) -> bool {
    matches!(value, "localhost" | "127.0.0.1" | "::1")
}

struct ServerConfig {
    host: String,
    port: u16,
    server_name: String,
    ca_der: Vec<u8>,
    username: String,
    password: String,
}

fn server_config(require_ca: bool) -> ServerConfig {
    let host = required(HOST);
    assert!(
        loopback_host(&host),
        "independent server host must be loopback, got {host:?}"
    );
    let server_name = required(SERVER_NAME);
    let port = required(PORT)
        .parse::<u16>()
        .expect("independent server port must be a u16");
    assert_ne!(port, 0, "independent server port must be non-zero");
    let username = required(USERNAME);
    let password = required(PASSWORD);
    assert!(
        username.ends_with("@localhost"),
        "synthetic mail account must use the localhost domain"
    );
    assert_eq!(password, "synthetic-only-password");
    let ca_der = if require_ca {
        fs::read(required(CA_DER_PATH)).expect("read one-run server test CA")
    } else {
        Vec::new()
    };
    ServerConfig {
        host,
        port,
        server_name,
        ca_der,
        username,
        password,
    }
}

async fn isolated_service(
    config: &ServerConfig,
    provider_id: &str,
) -> (tempfile::TempDir, Store, Account, MailService) {
    let profile = tempfile::tempdir().expect("create isolated Dakia profile");
    let store = Store::open(profile.path().join("dakia.db"))
        .await
        .expect("open isolated store");
    let account = Account {
        id: Uuid::new_v4(),
        email: config.username.clone(),
        account_name: "Independent synthetic integration".into(),
        display_name: "Independent synthetic integration".into(),
        provider_id: provider_id.into(),
        auth: AccountAuth::Password {
            username: config.username.clone(),
        },
        imap_host: config.host.clone(),
        imap_port: config.port,
        imap_security: Security::Tls,
        smtp_host: config.host.clone(),
        smtp_port: 1,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    store.save_account(&account).await.expect("persist account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, &config.password)
        .await
        .expect("persist synthetic credential in isolated profile");
    (profile, store, account, service)
}

/// Exercises the non-test production root store against an independent
/// server's one-run self-signed certificate. The typed error check prevents a
/// bad login, malformed configuration, or dead port from being counted as a
/// certificate-validation pass.
#[tokio::test]
#[ignore = "requires scripts/mail-safety-servers.sh and a loopback server"]
async fn external_server_rejects_untrusted_tls() {
    let config = server_config(false);
    let (_profile, _store, account, service) =
        isolated_service(&config, "independent-server").await;
    let error = service
        .imap_auth_probe(&account)
        .await
        .expect_err("production WebPKI roots must reject the independent self-signed certificate");
    let message = format!("{error:#}").to_ascii_lowercase();
    assert!(
        message.contains("tls handshake failed")
            && (message.contains("certificate")
                || message.contains("unknownissuer")
                || message.contains("unknown issuer")),
        "expected typed TLS certificate failure, got {message:?}"
    );
    assert!(!message.contains(&config.password));
}

/// Proves that the production TLS connector and sync state machine interoperate
/// with Dovecot using only a deliberately supplied one-run test CA. This is
/// ignored because it needs Docker and must never run against a real mailbox.
#[tokio::test]
#[ignore = "requires scripts/mail-safety-servers.sh and a loopback Dovecot container"]
async fn external_server_roundtrip() {
    let config = server_config(true);
    assert!(loopback_host(&config.server_name));
    let (profile, store, account, service) = isolated_service(&config, "independent-dovecot").await;
    let database = profile.path().join("dakia.db");

    let endpoint = MailService::imap_loopback_endpoint(
        config.host.clone(),
        config.port,
        config.server_name,
        config.ca_der,
    )
    .expect("construct explicit test-only trust root");
    let first = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, endpoint)
        .await
        .expect("production IMAP TLS connector and Dovecot sync");
    assert_eq!(first.synced_count, 0, "the synthetic INBOX starts empty");
    assert!(
        store
            .mailbox_uids(account.id, "INBOX")
            .await
            .expect("read synced mailbox")
            .is_empty(),
        "empty Dovecot INBOX must remain empty in Dakia's catalogue"
    );

    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen isolated profile");
    let accounts = reopened.accounts().await.expect("read persisted account");
    assert_eq!(accounts.len(), 1, "the isolated profile has one account");
    assert_eq!(accounts[0].id, account.id);
    assert_eq!(accounts[0].email, account.email);
    assert_eq!(accounts[0].imap_host, account.imap_host);
    assert_eq!(accounts[0].imap_port, account.imap_port);
    assert!(reopened
        .mailbox_uids(account.id, "INBOX")
        .await
        .expect("read durable empty mailbox")
        .is_empty());
}

/// Reads the governed mixed/alternative regression fixture from a real
/// Dovecot server through Dakia's production TLS, synchronization, selective
/// MIME fetch, persistence, and Reader projection path. The companion server
/// harness inspects Dovecot's decrypted post-login rawlog and rejects the run
/// if Dakia issues the full-message recovery command.
#[ignore = "requires scripts/mail-safety-servers.sh and a seeded loopback Dovecot container"]
#[tokio::test]
async fn external_dovecot_selective_mime_roundtrip() {
    const GOVERNED_FIXTURE: &[u8] = include_bytes!(
        "../testdata/mime/greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction.eml"
    );
    const EXPECTED_SUBJECT: &str = "Dakia fictional held-sync single-send 8VRoLm";
    const EXPECTED_BODY: &str =
        "One isolated fictional message for native send outcome acceptance. Do not resend.";

    assert_eq!(GOVERNED_FIXTURE.len(), 1052);
    assert!(GOVERNED_FIXTURE
        .windows(EXPECTED_SUBJECT.len())
        .any(|window| window == EXPECTED_SUBJECT.as_bytes()));
    let config = server_config(true);
    assert!(loopback_host(&config.server_name));
    let (profile, store, account, service) =
        isolated_service(&config, "independent-dovecot-selective-mime").await;
    let database = profile.path().join("dakia.db");

    let sync_endpoint = MailService::imap_loopback_endpoint(
        config.host.clone(),
        config.port,
        config.server_name.clone(),
        config.ca_der.clone(),
    )
    .expect("construct explicit Dovecot test-only trust root");
    let sync = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, sync_endpoint)
        .await
        .expect("sync the one governed fictional message from Dovecot");
    assert_eq!(
        sync.synced_count, 1,
        "the seeded Dovecot INBOX has one message"
    );

    let uids = store
        .mailbox_uids(account.id, "INBOX")
        .await
        .expect("read the synchronized Dovecot UID");
    assert_eq!(uids.len(), 1, "only the governed fixture may be present");
    let uid = *uids
        .iter()
        .next()
        .expect("one governed Dovecot fixture UID");
    let catalogue = store
        .message_by_locator(account.id, "INBOX", uid)
        .await
        .expect("read synchronized message locator")
        .expect("the governed fixture has a local catalogue row");
    assert_eq!(catalogue.subject, EXPECTED_SUBJECT);
    assert!(
        !catalogue.is_read,
        "catalogue sync must remain read-neutral"
    );
    assert!(catalogue.attachments.is_empty());
    assert!(!catalogue.has_attachments);

    let state = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .expect("read Dovecot mailbox identity")
        .expect("synchronization persists Dovecot UIDVALIDITY");
    assert!(state.uid_validity > 0);

    let hydrate_endpoint = MailService::imap_loopback_endpoint(
        config.host,
        config.port,
        config.server_name,
        config.ca_der,
    )
    .expect("reconnect through the production Dovecot TLS client");
    let hydrated = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            uid,
            state.uid_validity,
            hydrate_endpoint,
        )
        .await
        .expect("hydrate Reader content through production selective MIME fetch");

    assert_eq!(hydrated.content_state, "complete");
    assert!(
        !hydrated.is_read,
        "BODY.PEEK hydration must remain read-neutral"
    );
    assert!(hydrated.attachments.is_empty());
    assert!(!hydrated.has_attachments);
    assert!(
        hydrated.message_id.is_none(),
        "an absent provider Message-ID must remain absent"
    );

    let cached = store
        .cached_message_content(&hydrated.id)
        .await
        .expect("read durable Reader cache")
        .expect("selective hydration publishes complete Reader content");
    assert_eq!(cached.body_text.trim(), EXPECTED_BODY);
    assert!(cached
        .body_html
        .as_deref()
        .is_some_and(|html| html.trim() == EXPECTED_BODY));
    assert!(cached.attachments.is_empty());

    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen the isolated Reader profile");
    let reopened_message = reopened
        .message_by_locator(account.id, "INBOX", uid)
        .await
        .expect("read reopened message locator")
        .expect("the hydrated message survives restart");
    assert!(!reopened_message.is_read);
    assert!(reopened_message.attachments.is_empty());
    assert!(!reopened_message.has_attachments);
    let reopened_cached = reopened
        .cached_message_content(&reopened_message.id)
        .await
        .expect("read reopened Reader cache")
        .expect("complete selective Reader content survives restart");
    assert_eq!(reopened_cached.body_text.trim(), EXPECTED_BODY);
    assert!(reopened_cached
        .body_html
        .as_deref()
        .is_some_and(|html| html.trim() == EXPECTED_BODY));
    assert!(reopened_cached.attachments.is_empty());
}

/// Sends a synthetic message through GreenMail's real SMTP implementation,
/// saves its generic-provider Sent copy through IMAP, then reads the SMTP
/// delivery through IMAP. This uses no Gmail-only IMAP extensions.
#[tokio::test]
#[ignore = "requires scripts/mail-safety-servers.sh greenmail"]
async fn external_server_greenmail_roundtrip() {
    let config = server_config(true);
    let smtp_port = required(SMTP_PORT)
        .parse::<u16>()
        .expect("GreenMail SMTP port must be a u16");
    assert_ne!(smtp_port, 0);
    let (_profile, store, mut account, service) =
        isolated_service(&config, "independent-greenmail").await;
    account.smtp_port = smtp_port;
    store
        .save_account(&account)
        .await
        .expect("persist SMTP endpoint");

    let initial_endpoint = MailService::imap_loopback_endpoint(
        config.host.clone(),
        config.port,
        config.server_name.clone(),
        config.ca_der.clone(),
    )
    .expect("construct GreenMail IMAP endpoint");
    assert_eq!(
        service
            .sync_inbox_with_imap_endpoint_for_test(&account, 50, initial_endpoint)
            .await
            .expect("sync empty GreenMail INBOX")
            .synced_count,
        0
    );

    let smtp_endpoint = MailService::smtp_loopback_endpoint(
        config.host.clone(),
        smtp_port,
        config.server_name.clone(),
        config.ca_der.clone(),
    )
    .expect("construct GreenMail SMTP endpoint");
    let sent_endpoint = MailService::imap_loopback_endpoint(
        config.host.clone(),
        config.port,
        config.server_name.clone(),
        config.ca_der.clone(),
    )
    .expect("construct GreenMail Sent-copy endpoint");
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec![config.username.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Independent GreenMail delivery".into(),
        body_text: "synthetic round-trip body".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    service
        .send_with_endpoints_for_test(&account, &draft, smtp_endpoint, Some(sent_endpoint))
        .await
        .expect("submit synthetic SMTP message and persist one Sent copy");

    let delivered_endpoint = MailService::imap_loopback_endpoint(
        config.host,
        config.port,
        config.server_name,
        config.ca_der,
    )
    .expect("reconnect GreenMail IMAP endpoint");
    assert_eq!(
        service
            .sync_inbox_with_imap_endpoint_for_test(&account, 50, delivered_endpoint)
            .await
            .expect("sync GreenMail SMTP delivery")
            .synced_count,
        1,
        "the exact synthetic SMTP submission must appear once in IMAP"
    );
}
