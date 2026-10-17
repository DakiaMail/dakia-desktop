use chrono::{DateTime, Utc};
use dakia_core::{
    provider, Account, AccountDraft, CachedMessageContent, DraftService, MailService, MailSummary,
    Security, Store,
};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

const RESPONSE_DEADLINE: Duration = Duration::from_secs(5);

struct BridgeProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(profile: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dakia"))
            .args([
                "--data-dir",
                profile.to_str().expect("profile path is UTF-8"),
                "agent-bridge",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn private agent bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = child.stdout.take().expect("bridge stdout");
        Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
        }
    }

    async fn send(&mut self, request: Value) {
        let stdin = self.stdin.as_mut().expect("bridge stdin is open");
        stdin
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .expect("write bridge request");
        stdin
            .write_all(b"\n")
            .await
            .expect("terminate JSONL request");
        stdin.flush().await.expect("flush bridge request");
    }

    async fn send_raw(&mut self, request: &[u8]) {
        let stdin = self.stdin.as_mut().expect("bridge stdin is open");
        stdin
            .write_all(request)
            .await
            .expect("write raw bridge request");
        stdin
            .write_all(b"\n")
            .await
            .expect("terminate raw JSONL request");
        stdin.flush().await.expect("flush raw bridge request");
    }

    async fn response(&mut self) -> Value {
        let line = timeout(RESPONSE_DEADLINE, self.stdout.next_line())
            .await
            .expect("bridge response timed out")
            .expect("read bridge response")
            .expect("bridge exited before responding");
        serde_json::from_str(&line).expect("bridge response is JSON")
    }

    async fn request(&mut self, request: Value) -> Value {
        self.send(request).await;
        self.response().await
    }

    async fn close(mut self) {
        drop(self.stdin.take());
        let status = timeout(RESPONSE_DEADLINE, self.child.wait())
            .await
            .expect("bridge did not exit after stdin closed")
            .expect("wait for bridge exit");
        assert!(status.success(), "bridge exited unsuccessfully: {status}");
    }
}

fn lease_token(response: &Value) -> &str {
    response["result"]["lease_token"]
        .as_str()
        .expect("credential.acquire returns a lease token")
}

async fn seed_fictional_mail(profile: &Path) -> (Account, MailSummary, Account, MailSummary) {
    let store = Store::open(profile.join("dakia.db"))
        .await
        .expect("open fixture profile");
    let make_account = |label: &str| {
        AccountDraft {
            email: format!("{label}@example.test"),
            display_name: format!("{label} fixture"),
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
        .into_account(provider::by_id("custom").expect("custom provider preset"))
    };
    let insurance_account = make_account("insurance-owner");
    let personal_account = make_account("personal-owner");
    store
        .save_account(&insurance_account)
        .await
        .expect("save insurance account");
    store
        .save_account(&personal_account)
        .await
        .expect("save personal account");

    let make_message =
        |id: &str, account: &Account, uid: i64, sender: &str, subject: &str, body: &str| {
            MailSummary {
                id: id.into(),
                account_id: account.id.to_string(),
                mailbox: "INBOX".into(),
                uid,
                message_id: Some(format!("<{id}@example.test>")),
                in_reply_to: None,
                reference_ids: None,
                thread_id: format!("{id}-initial-thread"),
                subject: subject.into(),
                from_name: Some(sender.into()),
                from_address: format!("{}@example.test", sender.to_lowercase().replace(' ', ".")),
                to_addresses: account.email.clone(),
                cc_addresses: String::new(),
                bcc_addresses: String::new(),
                reply_to_addresses: String::new(),
                received_at: Utc::now(),
                snippet: body.into(),
                body_text: body.into(),
                body_html: None,
                content_state: "complete".into(),
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
        };
    let insurance_message = make_message(
        "home-insurance-update",
        &insurance_account,
        41,
        "Case Handler",
        "Home insurance claim inspection is booked for Monday.",
        "Home insurance claim inspection is booked for Monday.",
    );
    let personal_message = make_message(
        "john-invitation",
        &personal_account,
        7,
        "John Example",
        "Plans today",
        "Can you make it today?",
    );
    store
        .upsert_messages(&[insurance_message.clone(), personal_message.clone()])
        .await
        .expect("seed fictional messages and conversation identities");
    drop(store);
    (
        insurance_account,
        insurance_message,
        personal_account,
        personal_message,
    )
}

#[tokio::test]
#[ignore = "manual fixture helper for an isolated live-agent acceptance profile"]
async fn seed_live_agent_acceptance_fixture() {
    let profile = std::env::var_os("DAKIA_ACCEPTANCE_PROFILE")
        .map(std::path::PathBuf::from)
        .expect("set DAKIA_ACCEPTANCE_PROFILE to the isolated acceptance profile");
    let store = Store::open(profile.join("dakia.db"))
        .await
        .expect("open isolated acceptance profile");

    async fn fixture_account(store: &Store, email: &str, display_name: &str) -> Account {
        let existing = store
            .accounts()
            .await
            .expect("list fixture accounts")
            .into_iter()
            .find(|account| account.email == email);
        let mut account = existing.unwrap_or_else(|| {
            AccountDraft {
                email: email.to_owned(),
                display_name: display_name.to_owned(),
                provider_id: Some("custom".into()),
                username: Some(email.to_owned()),
                imap_host: Some("127.0.0.1".into()),
                imap_port: Some(1),
                imap_security: Some(Security::Tls),
                smtp_host: Some("127.0.0.1".into()),
                smtp_port: Some(1),
                smtp_security: Some(Security::Tls),
                archive_mailbox: Some("Archive".into()),
                spam_mailbox: Some("Spam".into()),
            }
            .into_account(provider::by_id("custom").expect("custom provider preset"))
        });
        account.account_name = display_name.to_owned();
        account.display_name = display_name.to_owned();
        account.imap_host = "127.0.0.1".into();
        account.imap_port = 1;
        account.imap_security = Security::Tls;
        account.smtp_host = "127.0.0.1".into();
        account.smtp_port = 1;
        account.smtp_security = Security::Tls;
        account.enabled = true;
        store
            .save_account(&account)
            .await
            .expect("save fixture account without changing provider credentials");
        account
    }

    fn fixture_message(
        id: &str,
        account: &Account,
        uid: i64,
        rfc_id: &str,
        in_reply_to: Option<&str>,
        references: Option<&str>,
        subject: &str,
        from_name: &str,
        from_address: &str,
        received_at: &str,
        body: &str,
    ) -> MailSummary {
        MailSummary {
            id: id.into(),
            account_id: account.id.to_string(),
            mailbox: "INBOX".into(),
            uid,
            message_id: Some(rfc_id.into()),
            in_reply_to: in_reply_to.map(str::to_owned),
            reference_ids: references.map(str::to_owned),
            thread_id: format!("{id}-fixture-thread"),
            subject: subject.into(),
            from_name: Some(from_name.into()),
            from_address: from_address.into(),
            to_addresses: account.email.clone(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: DateTime::parse_from_rfc3339(received_at)
                .expect("fixture timestamp")
                .with_timezone(&Utc),
            snippet: body.into(),
            body_text: body.into(),
            body_html: None,
            content_state: "complete".into(),
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

    let insurance = fixture_account(
        &store,
        "acceptance.insurance@example.test",
        "Fictional Home Insurance",
    )
    .await;
    let personal = fixture_account(
        &store,
        "acceptance.personal@example.test",
        "Fictional Personal Mail",
    )
    .await;
    let claim_opened = fixture_message(
        "acceptance-insurance-opened",
        &insurance,
        8101,
        "<acceptance-claim-opened@example.test>",
        None,
        None,
        "Home insurance claim HI-2048 opened",
        "Avery Claims",
        "avery.claims@example.test",
        "2026-10-10T09:15:00Z",
        "We opened fictional claim HI-2048. Please send the repair estimate.",
    );
    let inspection_booked = fixture_message(
        "acceptance-insurance-inspection",
        &insurance,
        8102,
        "<acceptance-inspection-booked@example.test>",
        Some("<acceptance-claim-opened@example.test>"),
        Some("<acceptance-claim-opened@example.test>"),
        "Re: Home insurance claim HI-2048 opened",
        "Avery Claims",
        "avery.claims@example.test",
        "2026-10-16T14:30:00Z",
        "The latest fictional update is that the home inspection is booked for 21 October 2026. We are still waiting for the repair estimate.",
    );
    let john = fixture_message(
        "acceptance-john-invitation",
        &personal,
        9101,
        "<acceptance-john-invitation@example.test>",
        None,
        None,
        "Plans today",
        "John Example",
        "john@example.test",
        "2026-10-17T08:00:00Z",
        "Hi, can you still make it today?",
    );
    let other_john = fixture_message(
        "acceptance-john-work-update",
        &insurance,
        8103,
        "<acceptance-john-work-update@example.test>",
        None,
        None,
        "Project schedule update",
        "John Other",
        "john.other@example.test",
        "2026-10-17T07:30:00Z",
        "The fictional project schedule has moved to next week.",
    );
    let messages = [claim_opened, inspection_booked, john, other_john];
    store
        .upsert_messages(&messages)
        .await
        .expect("seed fictional acceptance messages");
    for message in &messages {
        store
            .cache_message_content(
                &message.id,
                false,
                CachedMessageContent {
                    body_text: message.body_text.clone(),
                    body_html: None,
                    unsubscribe_kind: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("cache fictional message content for offline reads");
    }
    let drafts = DraftService::new(store.clone(), MailService::new(store.clone()))
        .list(None)
        .await
        .expect("snapshot acceptance draft baseline");

    println!(
        "{}",
        json!({
            "insurance_account_id": insurance.id,
            "personal_account_id": personal.id,
            "message_ids": messages.iter().map(|message| &message.id).collect::<Vec<_>>(),
            "draft_count_baseline": drafts.len(),
            "insurance_draft_count_baseline": drafts.iter().filter(|draft| draft.account_id == insurance.id).count(),
            "personal_draft_count_baseline": drafts.iter().filter(|draft| draft.account_id == personal.id).count()
        })
    );
}

#[tokio::test]
async fn credential_lock_is_cross_process_and_released_when_owner_exits() {
    let temporary = TempDir::new().expect("temporary profile parent");
    let profile = temporary.path().join("profile");
    let mut owner = BridgeProcess::spawn(&profile);
    let owner_lease = owner
        .request(json!({"id":"owner","method":"credential.acquire"}))
        .await;
    assert_eq!(owner_lease["id"], "owner");
    assert!(owner_lease.get("error").is_none());

    let mut waiter = BridgeProcess::spawn(&profile);
    waiter
        .send(json!({"id":"waiter","method":"credential.acquire"}))
        .await;
    assert!(
        timeout(Duration::from_millis(250), waiter.stdout.next_line())
            .await
            .is_err(),
        "a second process must wait while the first process owns the credential lock"
    );

    owner.close().await;
    let waiter_lease = waiter.response().await;
    assert_eq!(waiter_lease["id"], "waiter");
    assert!(waiter_lease.get("error").is_none());
    assert_ne!(lease_token(&owner_lease), lease_token(&waiter_lease));
    waiter.close().await;
}

#[tokio::test]
async fn wrong_and_released_lease_tokens_cannot_write_credentials() {
    let temporary = TempDir::new().expect("temporary profile parent");
    let profile = temporary.path().join("profile");
    let mut bridge = BridgeProcess::spawn(&profile);
    let acquired = bridge
        .request(json!({"id":1,"method":"credential.acquire"}))
        .await;
    let token = lease_token(&acquired).to_owned();

    let wrong = bridge
        .request(json!({
            "id":2,
            "method":"credential.write",
            "params":{
                "lease_token":"wrong-lease-token",
                "provider_id":"fixture",
                "credential":{"type":"api_key","key":"must-not-be-saved"}
            }
        }))
        .await;
    assert_eq!(wrong["error"]["code"], "operation_failed");

    let released = bridge
        .request(json!({
            "id":3,
            "method":"credential.release",
            "params":{"lease_token":token}
        }))
        .await;
    assert_eq!(released["result"], json!({"released":true}));
    let stale = bridge
        .request(json!({
            "id":4,
            "method":"credential.write",
            "params":{
                "lease_token":token,
                "provider_id":"fixture",
                "credential":{"type":"api_key","key":"must-not-be-saved"}
            }
        }))
        .await;
    assert_eq!(stale["error"]["code"], "operation_failed");
    let listed = bridge
        .request(json!({"id":5,"method":"credential.list"}))
        .await;
    assert_eq!(listed, json!({"id":5,"result":[]}));
    assert!(!listed.to_string().contains("must-not-be-saved"));
    bridge.close().await;
}

#[tokio::test]
async fn invalid_json_is_rejected_and_credential_list_never_discloses_secrets() {
    let temporary = TempDir::new().expect("temporary profile parent");
    let profile = temporary.path().join("profile");
    let mut bridge = BridgeProcess::spawn(&profile);
    bridge.send_raw(br#"{"id":1,"method":broken}"#).await;
    assert_eq!(
        bridge.response().await,
        json!({
            "id":null,
            "error":{"code":"invalid_request","message":"invalid JSONL request"}
        })
    );

    let acquired = bridge
        .request(json!({"id":2,"method":"credential.acquire"}))
        .await;
    let token = lease_token(&acquired).to_owned();
    let secret = "fixture-subscription-secret-never-list";
    let saved = bridge
        .request(json!({
            "id":3,
            "method":"credential.write",
            "params":{
                "lease_token":token,
                "provider_id":"chatgpt-subscription",
                "credential":{"type":"oauth","access_token":secret,"refresh_token":"refresh-secret"}
            }
        }))
        .await;
    assert_eq!(saved["result"], json!({"saved":true}));

    let listed = bridge
        .request(json!({"id":4,"method":"credential.list"}))
        .await;
    assert_eq!(
        listed,
        json!({
            "id":4,
            "result":[{"providerId":"chatgpt-subscription","type":"oauth"}]
        })
    );
    let serialized = listed.to_string();
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("refresh-secret"));
    bridge.close().await;
}

#[tokio::test]
async fn fictional_draft_is_account_bound_and_survives_bridge_restart_without_sending() {
    let temporary = TempDir::new().expect("temporary profile parent");
    let profile = temporary.path().join("profile");
    let (insurance_account, insurance_message, personal_account, _) =
        seed_fictional_mail(&profile).await;
    let mut bridge = BridgeProcess::spawn(&profile);

    let wrong_account_reply = bridge
        .request(json!({
            "id":"wrong-account",
            "method":"draft.save",
            "params":{
                "account_id":personal_account.id,
                "reply_to_message_id":insurance_message.id,
                "to":[{"email":"handler@example.test","name":"Case Handler"}],
                "subject":"Re: Home insurance update",
                "body_text":"Thank you for the update.",
                "attachments":[]
            }
        }))
        .await;
    assert_eq!(
        wrong_account_reply["error"]["code"], "operation_failed",
        "a message from another account must not become a reply target"
    );
    let personal_drafts = bridge
        .request(json!({
            "id":"personal-drafts",
            "method":"draft.list",
            "params":{"account_id":personal_account.id}
        }))
        .await;
    assert_eq!(personal_drafts["result"]["drafts"], json!([]));

    let created = bridge
        .request(json!({
            "id":"create-draft",
            "method":"draft.save",
            "params":{
                "account_id":insurance_account.id,
                "to":[{"email":"john@example.test","name":"John Example"}],
                "subject":"Today",
                "body_text":"I cannot make it today.",
                "attachments":[]
            }
        }))
        .await;
    assert!(created.get("error").is_none(), "{created}");
    let draft_id = created["result"]["draft"]["id"]
        .as_str()
        .expect("saved draft ID")
        .to_owned();
    assert_eq!(created["result"]["draft"]["revision"], 1);
    assert_eq!(
        created["result"]["draft"]["compose"]["account_id"],
        insurance_account.id.to_string()
    );
    assert_eq!(
        created["result"]["draft"]["compose"]["to"],
        json!(["\"John Example\" <john@example.test>"])
    );
    assert_eq!(
        created["result"]["draft"]["compose"]["body_text"],
        "I cannot make it today."
    );
    bridge.close().await;

    let mut restarted = BridgeProcess::spawn(&profile);
    let loaded = restarted
        .request(json!({
            "id":"load-after-restart",
            "method":"draft.get",
            "params":{"draft_id":draft_id}
        }))
        .await;
    assert_eq!(loaded["result"]["draft"]["id"], draft_id);
    assert_eq!(
        loaded["result"]["draft"]["compose"]["account_id"],
        insurance_account.id.to_string()
    );
    assert_eq!(loaded["result"]["draft"]["compose"]["subject"], "Today");
    assert_eq!(
        loaded["result"]["draft"]["compose"]["body_text"],
        "I cannot make it today."
    );
    assert_eq!(loaded["result"]["draft"]["revision"], 1);
    let actions = restarted
        .request(json!({"id":"actions","method":"action.list"}))
        .await;
    assert_eq!(
        actions["result"]["actions"],
        json!([]),
        "saving and reading a draft must never create a send attempt"
    );
    restarted.close().await;
}

#[tokio::test]
async fn thread_context_is_account_scoped_versioned_restorable_and_restart_safe() {
    let temporary = TempDir::new().expect("temporary profile parent");
    let profile = temporary.path().join("profile");
    let (insurance_account, insurance_message, personal_account, personal_message) =
        seed_fictional_mail(&profile).await;
    let mut bridge = BridgeProcess::spawn(&profile);

    let initial = bridge
        .request(json!({
            "id":"context-initial",
            "method":"context.get",
            "params":{
                "account_id":insurance_account.id,
                "target":"message",
                "target_id":insurance_message.id,
                "kind":"notes"
            }
        }))
        .await;
    assert_eq!(initial["result"]["body"], "");
    assert_eq!(initial["result"]["revision"], 0);

    let first = bridge
        .request(json!({
            "id":"context-first",
            "method":"context.set",
            "params":{
                "account_id":insurance_account.id,
                "target":"message",
                "target_id":insurance_message.id,
                "kind":"notes",
                "body":"Inspection is booked for Monday.",
                "expected_revision":0
            }
        }))
        .await;
    assert_eq!(first["result"]["revision"], 1);
    let conversation_id = first["result"]["conversation_id"].clone();

    let wrong_account = bridge
        .request(json!({
            "id":"context-wrong-account",
            "method":"context.get",
            "params":{
                "account_id":personal_account.id,
                "target":"message",
                "target_id":insurance_message.id,
                "kind":"notes"
            }
        }))
        .await;
    assert_eq!(wrong_account["error"]["code"], "operation_failed");
    let personal_initial = bridge
        .request(json!({
            "id":"personal-context",
            "method":"context.get",
            "params":{
                "account_id":personal_account.id,
                "target":"message",
                "target_id":personal_message.id,
                "kind":"notes"
            }
        }))
        .await;
    assert_eq!(personal_initial["result"]["body"], "");
    assert_ne!(
        personal_initial["result"]["conversation_id"],
        conversation_id
    );

    let second = bridge
        .request(json!({
            "id":"context-second",
            "method":"context.set",
            "params":{
                "account_id":insurance_account.id,
                "target":"conversation",
                "target_id":conversation_id,
                "kind":"notes",
                "body":"Inspection completed. Waiting for the assessor report.",
                "expected_revision":1
            }
        }))
        .await;
    assert_eq!(second["result"]["revision"], 2);
    let history = bridge
        .request(json!({
            "id":"context-history",
            "method":"context.history",
            "params":{
                "account_id":insurance_account.id,
                "target":"conversation",
                "target_id":conversation_id,
                "kind":"notes"
            }
        }))
        .await;
    let revisions = history["result"]["history"]
        .as_array()
        .expect("context history array");
    assert_eq!(revisions.len(), 2);
    let first_revision = revisions
        .iter()
        .find(|revision| revision["revision"] == 1)
        .expect("first context revision");
    assert_eq!(first_revision["body"], "Inspection is booked for Monday.");

    let restored = bridge
        .request(json!({
            "id":"context-restore",
            "method":"context.restore",
            "params":{
                "account_id":insurance_account.id,
                "target":"conversation",
                "target_id":conversation_id,
                "kind":"notes",
                "revision":first_revision["id"],
                "expected_revision":2
            }
        }))
        .await;
    assert_eq!(
        restored["result"]["body"],
        "Inspection is booked for Monday."
    );
    assert_eq!(restored["result"]["revision"], 3);
    bridge.close().await;

    let mut restarted = BridgeProcess::spawn(&profile);
    let after_restart = restarted
        .request(json!({
            "id":"context-after-restart",
            "method":"context.get",
            "params":{
                "account_id":insurance_account.id,
                "target":"conversation",
                "target_id":conversation_id,
                "kind":"notes"
            }
        }))
        .await;
    assert_eq!(
        after_restart["result"]["body"],
        "Inspection is booked for Monday."
    );
    assert_eq!(after_restart["result"]["revision"], 3);
    restarted.close().await;
}
