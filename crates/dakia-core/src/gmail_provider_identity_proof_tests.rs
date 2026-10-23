//! Manually authorized, bounded Gmail provider-identity proof.
//!
//! This module is ignored because it contacts the production Gmail IMAP TLS
//! endpoint. The checkout-owned wrapper supplies the two private, read-only
//! locator observations and credentials. This test never creates an SMTP
//! client, changes a mailbox flag, or writes provider content outside its
//! private temporary SQLite profile.

use crate::{
    account::{Account, AccountAuth},
    provider::Security,
    storage::{ConversationTarget, MailSummary},
    MailService, Store,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const ROUNDTRIP_ID: &str = "45bacd384f0c4a74893f14e4fea23d9a";
const ACCOUNT_ID: &str = "b0040000-0000-4000-8000-000000000014";
const REDACTED_ADDRESS: &str = "provider-proof@example.invalid";
const INPUT_PATH_ENV: &str = "DAKIA_GMAIL_PROVIDER_IDENTITY_INPUT";
const OUTPUT_PATH_ENV: &str = "DAKIA_GMAIL_PROVIDER_IDENTITY_OUTPUT";
const SOURCE_DIGEST_ENV: &str = "DAKIA_GMAIL_PROVIDER_IDENTITY_SOURCE_DIGEST";
const RUN_AUTHORIZATION_ENV: &str = "DAKIA_GMAIL_PROVIDER_IDENTITY_AUTHORIZED";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProofInput {
    schema_version: u8,
    roundtrip_id: String,
    expected_message_id: String,
    observations: Vec<MailboxObservation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MailboxObservation {
    local_mailbox: String,
    remote_wire_name: String,
    uid: u32,
    uid_validity: u32,
    exists: u32,
    flags_unchanged: bool,
    gmail_message_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofEvidence<'a> {
    schema_version: u8,
    outcome: &'a str,
    result_code: &'a str,
    source_digest: &'a str,
    provider_scope: &'a str,
    roundtrip_id: &'a str,
    production_full_fetch: bool,
    parser_verified: bool,
    profile_schema_version: u8,
    rfc_message_id_matched_across_copies: bool,
    provider_identity_matched_across_copies: bool,
    provider_identity_selected_thread_key: bool,
    provider_identity_survived_reopen: bool,
    label_copies_deduplicated: bool,
    epoch_fenced_store: bool,
    cache_published: bool,
    store_reopen_verified: bool,
    physical_copies: u8,
    display_representatives: u8,
    distinct_physical_locators: bool,
    expected_subject_sha256: String,
    expected_body_sha256: String,
    limitations: [&'a str; 4],
}

fn fixture() -> String {
    format!("Dakia disposable provider roundtrip {ROUNDTRIP_ID}")
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).map_err(|_| anyhow!("required proof input is unavailable"))?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        bail!("required proof input is invalid");
    }
    Ok(value)
}

fn canonical_message_id(value: &str) -> Option<&str> {
    if value.len() >= 3
        && value.is_ascii()
        && value.starts_with('<')
        && value.ends_with('>')
        && !value[1..value.len() - 1]
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '<' | '>'))
    {
        Some(value)
    } else {
        None
    }
}

fn read_input(path: &Path) -> Result<ProofInput> {
    let bytes = fs::read(path).map_err(|_| anyhow!("private proof input is unavailable"))?;
    if bytes.len() > 16 * 1024 {
        bail!("private proof input exceeds its bound");
    }
    let input: ProofInput =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("private proof input is invalid"))?;
    validate_input(&input)?;
    Ok(input)
}

fn validate_input(input: &ProofInput) -> Result<()> {
    if input.schema_version != 1
        || input.roundtrip_id != ROUNDTRIP_ID
        || canonical_message_id(&input.expected_message_id).is_none()
        || input.observations.len() != 2
    {
        bail!("private proof input does not describe the approved target");
    }
    let mut roles: HashSet<&str> = HashSet::new();
    let mut wire_names = HashSet::new();
    let mut gmail_ids = HashSet::new();
    for observation in &input.observations {
        if !matches!(observation.local_mailbox.as_str(), "INBOX" | "Sent")
            || !roles.insert(observation.local_mailbox.as_str())
            || observation.remote_wire_name.is_empty()
            || !observation.remote_wire_name.is_ascii()
            || !wire_names.insert(&observation.remote_wire_name)
            || observation.uid == 0
            || observation.uid_validity == 0
            || observation.exists == 0
            || !observation.flags_unchanged
            || observation.gmail_message_id.is_empty()
            || observation.gmail_message_id.len() > 20
            || !observation
                .gmail_message_id
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            bail!("private proof input has an unsafe mailbox observation");
        }
        gmail_ids.insert(&observation.gmail_message_id);
    }
    if roles != HashSet::from(["INBOX", "Sent"]) || gmail_ids.len() != 1 {
        bail!("private proof input does not prove two Gmail label copies");
    }
    Ok(())
}

fn live_account(address: String) -> Result<Account> {
    if !address.ends_with("@gmail.com") || address.len() > 254 {
        bail!("authorized account is invalid");
    }
    Ok(Account {
        id: Uuid::parse_str(ACCOUNT_ID).context("fixed proof account id is invalid")?,
        email: address.clone(),
        account_name: address.clone(),
        display_name: "B004 disposable provider proof".into(),
        provider_id: "gmail".into(),
        auth: AccountAuth::Password { username: address },
        imap_host: "imap.gmail.com".into(),
        imap_port: 993,
        imap_security: Security::Tls,
        // No SMTP client is constructed in this proof.
        smtp_host: "smtp.invalid".into(),
        smtp_port: 0,
        smtp_security: Security::Tls,
        archive_mailbox: "[Gmail]/All Mail".into(),
        spam_mailbox: "[Gmail]/Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    })
}

fn redacted_account(account: &Account) -> Account {
    let mut redacted = account.clone();
    redacted.email = REDACTED_ADDRESS.into();
    redacted.account_name = REDACTED_ADDRESS.into();
    redacted.display_name = "B004 disposable provider proof".into();
    redacted.auth = AccountAuth::Password {
        username: REDACTED_ADDRESS.into(),
    };
    redacted
}

fn redact_message_addresses(message: &mut MailSummary) {
    message.from_name = None;
    message.from_address = REDACTED_ADDRESS.into();
    message.to_addresses = REDACTED_ADDRESS.into();
    message.cc_addresses.clear();
    message.bcc_addresses.clear();
    message.reply_to_addresses.clear();
}

fn verify_fetched_message(
    message: &MailSummary,
    observation: &MailboxObservation,
    expected_id: &str,
) -> Result<()> {
    let expected = fixture();
    if message.subject != expected
        || message.body_text != expected
        || message.message_id.as_deref() != Some(expected_id)
        || message.gmail_message_id.as_deref() != Some(observation.gmail_message_id.as_str())
        || message.in_reply_to.is_some()
        || message.reference_ids.is_some()
        || message.mailbox != observation.local_mailbox
        || message.uid != i64::from(observation.uid)
        || message.mailbox_uid_validity != Some(i64::from(observation.uid_validity))
    {
        bail!("production fetch did not match the approved message");
    }
    Ok(())
}

async fn assert_deduplicated_conversation(
    store: &Store,
    account: &Account,
    input: &ProofInput,
    fetched: &[MailSummary],
) -> Result<()> {
    let conversation = store
        .conversation_for_target(&ConversationTarget {
            account_id: account.id,
            local_message_id: None,
            rfc_message_id: Some(input.expected_message_id.clone()),
            thread_id: None,
            mailbox: None,
        })
        .await?
        .ok_or_else(|| anyhow!("deduplicated proof conversation is missing"))?;
    let gmail_id = &input.observations[0].gmail_message_id;
    let expected_ids = fetched
        .iter()
        .map(|message| &message.id)
        .collect::<HashSet<_>>();
    let actual_ids = conversation
        .source_messages
        .iter()
        .map(|message| &message.id)
        .collect::<HashSet<_>>();
    let mailboxes = conversation
        .source_messages
        .iter()
        .map(|message| message.mailbox.as_str())
        .collect::<HashSet<_>>();
    if conversation.thread_id != format!("gmail:{gmail_id}")
        || conversation.source_messages.len() != 2
        || conversation.messages.len() != 1
        || conversation.message_count != 1
        || expected_ids.len() != 2
        || expected_ids != actual_ids
        || mailboxes != HashSet::from(["INBOX", "Sent"])
        || conversation.source_messages.iter().any(|message| {
            message.account_id != account.id.to_string()
                || message.message_id.as_deref() != Some(input.expected_message_id.as_str())
                || message.gmail_message_id.is_some()
        })
    {
        bail!("Gmail label copies did not remain one safe display conversation");
    }
    Ok(())
}

async fn profile_schema_version(database: &Path) -> Result<u8> {
    let pool =
        sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", database.display())).await?;
    let version: String =
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = 'profile_schema_version'")
            .fetch_one(&pool)
            .await?;
    let migration_target: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_meta WHERE key = 'profile_schema_migration_target'",
    )
    .fetch_optional(&pool)
    .await?;
    if migration_target.is_some() || version != "3" {
        bail!("private proof profile schema is not the required completed schema 3");
    }
    Ok(3)
}

fn write_evidence(path: PathBuf, source_digest: &str, profile_schema_version: u8) -> Result<()> {
    let fixture = fixture();
    let evidence = ProofEvidence {
        schema_version: 2,
        outcome: "passed",
        result_code: "b004_production_gmail_identity_store_reopen_label_dedup",
        source_digest,
        provider_scope: "dedicated_disposable_gmail",
        roundtrip_id: ROUNDTRIP_ID,
        production_full_fetch: true,
        parser_verified: true,
        profile_schema_version,
        rfc_message_id_matched_across_copies: true,
        provider_identity_matched_across_copies: true,
        provider_identity_selected_thread_key: true,
        provider_identity_survived_reopen: true,
        label_copies_deduplicated: true,
        epoch_fenced_store: true,
        cache_published: true,
        store_reopen_verified: true,
        physical_copies: 2,
        display_representatives: 1,
        distinct_physical_locators: true,
        expected_subject_sha256: sha256(&fixture),
        expected_body_sha256: sha256(&fixture),
        limitations: [
            "does_not_exercise_sync_all_or_snapshot_finalization",
            "does_not_validate_dakia_list_parser_or_mailbox_plan_resolution",
            "live_copies_share_both_rfc_and_gmail_ids_but_thread_key_proves_gmail_identity_precedence",
            "does_not_prove_reader_frontend_or_native_webview_rendering",
        ],
    };
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .context("private evidence output cannot be created")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    output
        .write_all(&bytes)
        .context("private evidence output cannot be written")?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the checkout-owned read-only Gmail proof wrapper and explicit authorization"]
async fn production_gmail_provider_identity_store_reopen_label_dedup_is_read_only() -> Result<()> {
    if required_env(RUN_AUTHORIZATION_ENV)?.as_str() != "approved-read-only-b004" {
        bail!("Gmail provider proof is not explicitly authorized");
    }
    let input = read_input(Path::new(&required_env(INPUT_PATH_ENV)?))?;
    let source_digest = required_env(SOURCE_DIGEST_ENV)?;
    if !source_digest.bytes().all(|byte| byte.is_ascii_hexdigit()) || source_digest.len() != 64 {
        bail!("source manifest digest is invalid");
    }
    let account = live_account(required_env("GMAIL_ACCOUNT")?)?;
    let password = required_env("GMAIL_PASSWORD")?;
    let profile = tempfile::tempdir().context("private proof profile cannot be created")?;
    let database = profile.path().join("provider-proof.sqlite");
    let store = Store::open(&database).await?;
    let initial_profile_schema_version = profile_schema_version(&database).await?;
    store.save_account(&redacted_account(&account)).await?;
    for observation in &input.observations {
        store
            .save_mailbox_catalog_state(
                account.id,
                &observation.local_mailbox,
                &observation.remote_wire_name,
                observation.uid_validity,
                observation.exists as usize,
                false,
            )
            .await?;
    }
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, &password)
        .await?;
    let mut fetched = Vec::with_capacity(2);
    for observation in &input.observations {
        let mut message = service
            .fetch_full_message(&account, &observation.local_mailbox, observation.uid)
            .await
            .map_err(|_| anyhow!("production full-message fetch failed"))?;
        verify_fetched_message(&message, observation, &input.expected_message_id)?;
        redact_message_addresses(&mut message);
        fetched.push(message);
    }
    for (observation, message) in input.observations.iter().zip(&fetched) {
        store
            .upsert_catalog_messages_at_epoch(
                account.id,
                &observation.local_mailbox,
                i64::from(observation.uid_validity),
                std::slice::from_ref(message),
            )
            .await?;
    }
    for message in &fetched {
        let target = store
            .message(&message.id)
            .await?
            .ok_or_else(|| anyhow!("catalogue row is missing"))?;
        if !store
            .publish_message_content_at_epoch(&target, message)
            .await?
        {
            bail!("epoch-fenced content publication was rejected");
        }
        let cached = store
            .cached_message_content(&message.id)
            .await?
            .ok_or_else(|| anyhow!("published content cache is missing"))?;
        if cached.body_text != fixture() {
            bail!("published content cache differs from the approved message");
        }
    }
    store.finish_threading_backfill(account.id).await?;
    assert_deduplicated_conversation(&store, &account, &input, &fetched).await?;
    drop(service);
    drop(store);
    let reopened = Store::open(&database).await?;
    if profile_schema_version(&database).await? != initial_profile_schema_version {
        bail!("reopened proof profile changed its schema version");
    }
    assert_deduplicated_conversation(&reopened, &account, &input, &fetched).await?;
    for message in &fetched {
        let cached = reopened
            .cached_message_content(&message.id)
            .await?
            .ok_or_else(|| anyhow!("reopened content cache is missing"))?;
        if cached.body_text != fixture() {
            bail!("reopened content cache differs from the approved message");
        }
    }
    write_evidence(
        PathBuf::from(required_env(OUTPUT_PATH_ENV)?),
        &source_digest,
        initial_profile_schema_version,
    )?;
    Ok(())
}

#[test]
fn proof_input_rejects_extra_roles_changed_flags_and_identity_mismatches() {
    let mut input = ProofInput {
        schema_version: 1,
        roundtrip_id: ROUNDTRIP_ID.into(),
        expected_message_id: "<fixture@example.test>".into(),
        observations: vec![
            MailboxObservation {
                local_mailbox: "INBOX".into(),
                remote_wire_name: "INBOX".into(),
                uid: 4,
                uid_validity: 17,
                exists: 1,
                flags_unchanged: true,
                gmail_message_id: "9001".into(),
            },
            MailboxObservation {
                local_mailbox: "Sent".into(),
                remote_wire_name: "[Gmail]/Sent Mail".into(),
                uid: 9,
                uid_validity: 19,
                exists: 1,
                flags_unchanged: true,
                gmail_message_id: "9001".into(),
            },
        ],
    };
    assert!(validate_input(&input).is_ok());
    input.observations[1].flags_unchanged = false;
    assert!(validate_input(&input).is_err());
    input.observations[1].flags_unchanged = true;
    input.observations[1].gmail_message_id = "9002".into();
    assert!(validate_input(&input).is_err());
}

#[test]
fn wrapper_generated_input_contract_decodes_without_unknown_fields() {
    let directory = tempfile::tempdir().expect("private wrapper fixture directory");
    let path = directory.path().join("wrapper-input.json");
    std::fs::write(
        &path,
        r#"{"schemaVersion":1,"roundtripId":"45bacd384f0c4a74893f14e4fea23d9a","expectedMessageId":"<fixture@example.test>","observations":[{"localMailbox":"INBOX","remoteWireName":"INBOX","uid":4,"uidValidity":17,"exists":1,"flagsUnchanged":true,"gmailMessageId":"9001"},{"localMailbox":"Sent","remoteWireName":"[Gmail]/Sent Mail","uid":9,"uidValidity":19,"exists":1,"flagsUnchanged":true,"gmailMessageId":"9001"}]}"#,
    )
    .expect("write wrapper fixture");
    let input = read_input(&path).expect("wrapper fixture decodes through the production contract");
    assert_eq!(input.observations.len(), 2);
}
