//! Manual smoke against the checkout-owned 30,000-message fictional TLS
//! fixture. The runner supplies a one-run loopback CA through environment
//! variables. This module cannot use a system or public trust root.

use crate::{Account, AccountAuth, MailService, Security, Store};
use chrono::Utc;
use std::{env, fs, path::PathBuf};
use uuid::Uuid;

const HOST: &str = "DAKIA_INDEPENDENT_SERVER_HOST";
const PORT: &str = "DAKIA_INDEPENDENT_SERVER_PORT";
const SERVER_NAME: &str = "DAKIA_INDEPENDENT_SERVER_NAME";
const CA_DER_PATH: &str = "DAKIA_INDEPENDENT_CA_DER_PATH";
const USERNAME: &str = "DAKIA_INDEPENDENT_USERNAME";
const PASSWORD: &str = "DAKIA_INDEPENDENT_PASSWORD";
const PROFILE_ROOT: &str = "DAKIA_LARGE_MAILBOX_SMOKE_ROOT";
const ACCOUNT_ID: &str = "21c9e79c-c5b8-4d50-9d1e-6ba3d5323000";

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} is required for the large-mailbox TLS smoke"))
}

fn loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

struct FixtureConfig {
    host: String,
    port: u16,
    server_name: String,
    ca_der: Vec<u8>,
    username: String,
    password: String,
    profile: PathBuf,
}

fn fixture_config() -> FixtureConfig {
    let host = required(HOST);
    assert!(
        loopback_host(&host),
        "fixture host must be loopback, got {host:?}"
    );
    let server_name = required(SERVER_NAME);
    assert!(
        loopback_host(&server_name),
        "fixture server name must be loopback, got {server_name:?}"
    );
    let port = required(PORT)
        .parse::<u16>()
        .expect("fixture port must be a u16");
    assert_ne!(port, 0, "fixture port must be non-zero");
    let username = required(USERNAME);
    let password = required(PASSWORD);
    assert_eq!(username, "fictional-reader@localhost");
    assert_eq!(password, "synthetic-only-password");
    let profile = PathBuf::from(required(PROFILE_ROOT));
    assert!(
        profile.is_absolute(),
        "fixture profile must be an absolute temporary path"
    );
    FixtureConfig {
        host,
        port,
        server_name,
        ca_der: fs::read(required(CA_DER_PATH)).expect("read one-run loopback CA"),
        username,
        password,
        profile,
    }
}

/// This accepts a CA only through the crate's test-only endpoint constructor.
/// Production account connection construction still trusts WebPKI roots only.
#[tokio::test]
#[ignore = "requires scripts/run-large-mailbox-tls-smoke.sh and a fictional loopback TLS server"]
async fn large_mailbox_tls_smoke() {
    let config = fixture_config();
    fs::create_dir_all(&config.profile).expect("create isolated fixture profile");
    let database = config.profile.join("dakia.db");
    assert!(
        !database.exists(),
        "fixture runner must provide a fresh profile"
    );
    let store = Store::open(&database).await.expect("open isolated Store");
    let account = Account {
        id: Uuid::parse_str(ACCOUNT_ID).expect("fixed fictional account UUID"),
        email: config.username.clone(),
        account_name: "Fictional large mailbox".into(),
        display_name: "Fictional large mailbox".into(),
        provider_id: "custom".into(),
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
    store
        .save_account(&account)
        .await
        .expect("persist fictional account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, &config.password)
        .await
        .expect("persist fictional fixture credential");

    let snapshot_endpoint = MailService::imap_loopback_endpoint(
        config.host.clone(),
        config.port,
        config.server_name.clone(),
        config.ca_der.clone(),
    )
    .expect("construct explicit loopback test trust root");
    let synced = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, snapshot_endpoint)
        .await
        .expect("production TLS connector synchronizes the deterministic 30k fixture");
    assert_eq!(synced.synced_count, 30_000);
    assert_eq!(
        store
            .mailbox_uids(account.id, "INBOX")
            .await
            .expect("read durable membership")
            .len(),
        30_000
    );
    let mailbox = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .expect("read mailbox identity")
        .expect("sync persists INBOX identity");
    assert_eq!(mailbox.uid_validity, 77);
    assert_eq!(mailbox.remote_total, 30_000);
    assert!(mailbox.historical_complete);

    let reader_endpoint = MailService::imap_loopback_endpoint(
        config.host,
        config.port,
        config.server_name,
        config.ca_der,
    )
    .expect("construct Reader loopback test trust root");
    let hydrated = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            2,
            mailbox.uid_validity,
            reader_endpoint,
        )
        .await
        .expect("production Reader MIME path hydrates the retained multipart message");
    assert_eq!(hydrated.content_state, "complete");
    assert!(
        hydrated.is_read,
        "Reader BODY.PEEK hydration must preserve the synchronized Seen state"
    );
    let cached = store
        .cached_message_content(&hydrated.id)
        .await
        .expect("read Reader cache")
        .expect("Reader hydration persists cache");
    assert_eq!(
        cached.body_text.trim(),
        "Fictional retained multipart text."
    );
    assert!(cached.body_html.is_none());
    assert!(cached.attachments.is_empty());
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen Store after Reader hydration");
    let reopened_message = reopened
        .message_by_locator(account.id, "INBOX", 2)
        .await
        .expect("read retained message")
        .expect("retained Reader summary survives reopen");
    assert_eq!(
        reopened_message.subject,
        "Fictional retained multipart Reader message"
    );
    let reopened_cache = reopened
        .cached_message_content(&reopened_message.id)
        .await
        .expect("read reopened Reader cache")
        .expect("Reader cache survives reopen");
    assert_eq!(
        reopened_cache.body_text.trim(),
        "Fictional retained multipart text."
    );
    assert_eq!(reopened_cache.body_text, cached.body_text);
    fs::write(
        config.profile.join("large-mailbox-tls-smoke.json"),
        serde_json::to_vec(&serde_json::json!({
            "account_id": account.id,
            "message_count": 30_000,
            "uid_validity": 77,
            "reader_uid": 2,
            "reader_body_text": reopened_cache.body_text,
        }))
        .expect("serialize redacted smoke receipt"),
    )
    .expect("write redacted smoke receipt");
}
