//! Private JSONL transport used by the bundled agent. Never emit human output here.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use dakia_core::drafts::{DraftService, DraftSyncStatus, LocalDraft};
use dakia_core::mail::ComposeAttachment;
use dakia_core::thread_context::{ThreadContextKind, ThreadContextLocator};
use dakia_core::{storage::ConversationTarget, MailService, SearchQuery, Store};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const CREDENTIAL_KEY: &str = "dakia.agent.providers";
const MAX_REQUEST_BYTES: usize = 80 * 1024 * 1024;

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

struct Bridge {
    store: Store,
    profile: PathBuf,
    credential_lease: Option<(String, File, Instant)>,
    approvals: HashMap<String, Approval>,
    staged_attachments: HashMap<String, StagedAttachment>,
}

struct StagedAttachment {
    attachment: ComposeAttachment,
    touched: Instant,
}

struct Approval {
    nonce: String,
    hash: String,
    draft_id: String,
    expires: Instant,
    granted: bool,
    snapshot: Value,
}

pub async fn run(profile: PathBuf) -> Result<()> {
    let store = Store::open(profile.join("dakia.db")).await?;
    let mut bridge = Bridge {
        store,
        profile,
        credential_lease: None,
        approvals: HashMap::new(),
        staged_attachments: HashMap::new(),
    };
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    // Read bounded records without allocating an unbounded malicious line.
    loop {
        let mut record = Vec::new();
        let mut oversized = false;
        loop {
            let available = input.fill_buf().await?;
            if available.is_empty() {
                break;
            }
            let n = available
                .iter()
                .position(|b| *b == b'\n')
                .map_or(available.len(), |p| p + 1);
            let complete = available[n - 1] == b'\n';
            if record.len() + n > MAX_REQUEST_BYTES {
                oversized = true;
            }
            if !oversized {
                record.extend_from_slice(&available[..n]);
            }
            input.consume(n);
            if complete {
                break;
            }
        }
        if record.is_empty() && !oversized {
            break;
        }
        let response = if oversized {
            json!({"id":null,"error":{"code":"invalid_request","message":"request is too large"}})
        } else {
            match serde_json::from_slice::<Request>(&record) {
                Ok(request) => {
                    let result = bridge.dispatch(&request.method, request.params).await;
                    match result {
                        Ok(value) => json!({"id":request.id,"result":value}),
                        Err(error) => {
                            json!({"id":request.id,"error":{"code":"operation_failed","message":error.to_string()}})
                        }
                    }
                }
                Err(_) => {
                    json!({"id":null,"error":{"code":"invalid_request","message":"invalid JSONL request"}})
                }
            }
        };
        output
            .write_all(serde_json::to_string(&response)?.as_bytes())
            .await?;
        output.write_all(b"\n").await?;
        output.flush().await?;
    }
    Ok(())
}

impl Bridge {
    fn prepare_approval(
        &mut self,
        draft_id: &str,
        snapshot: &Value,
        binding: Value,
    ) -> Result<Value> {
        self.approvals
            .retain(|_, approval| approval.expires > Instant::now());
        let action_id = Uuid::new_v4().to_string();
        let nonce = Uuid::new_v4().to_string();
        let hash = snapshot_hash(snapshot)?;
        self.approvals.insert(
            action_id.clone(),
            Approval {
                nonce: nonce.clone(),
                hash: hash.clone(),
                draft_id: draft_id.to_owned(),
                expires: Instant::now() + Duration::from_secs(600),
                granted: false,
                snapshot: snapshot.clone(),
            },
        );
        Ok(json!({"action_id":action_id,"nonce":nonce,"hash":hash,"binding":binding}))
    }

    fn grant_approval(&mut self, params: &Value) -> Result<Value> {
        let action_id = string(params, "action_id")?;
        let approval = self
            .approvals
            .get_mut(action_id)
            .context("approval not found")?;
        if approval.expires <= Instant::now()
            || approval.nonce != string(params, "nonce")?
            || approval.hash != string(params, "hash")?
        {
            bail!("approval is expired or does not match this action");
        }
        approval.granted = true;
        Ok(json!({"approved":true}))
    }

    fn consume_approval(
        &mut self,
        draft_id: &str,
        params: &Value,
        snapshot: &Value,
    ) -> Result<String> {
        let action_id = string(params, "action_id")?;
        let approval = self
            .approvals
            .get(action_id)
            .context("approval is required")?;
        if !approval.granted
            || approval.expires <= Instant::now()
            || approval.draft_id != draft_id
            || approval.nonce != string(params, "nonce")?
            || approval.hash != string(params, "hash")?
            || approval.hash != snapshot_hash(snapshot)?
        {
            bail!("approval does not match the current draft; review and approve it again");
        }
        self.approvals.remove(action_id);
        Ok(action_id.to_owned())
    }

    async fn dispatch(&mut self, method: &str, params: Value) -> Result<Value> {
        match method {
            "account.list" => Ok(
                json!({"accounts":self.store.accounts().await?.into_iter().map(|a|json!({"id":a.id,"email":a.email,"display_name":a.display_name,"enabled":a.enabled})).collect::<Vec<_>>()}),
            ),
            "mail.sync" => {
                let ids: Vec<Uuid> = serde_json::from_value(
                    params.get("account_ids").cloned().unwrap_or(json!([])),
                )?;
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(250)
                    .clamp(1, 1000) as u32;
                let mut results = Vec::new();
                for account in self
                    .store
                    .accounts()
                    .await?
                    .into_iter()
                    .filter(|a| a.enabled && (ids.is_empty() || ids.contains(&a.id)))
                {
                    let count = MailService::new(self.store.clone())
                        .sync_all(&account, limit)
                        .await;
                    let drafts = self.drafts().reconcile_pending(&account, 20).await;
                    match count {
                        Ok(count) => results.push(json!({"account_id":account.id,"synced":count,"status":"refreshed","drafts":drafts.ok()})),
                        Err(_) => results.push(json!({"account_id":account.id,"status":"failed","drafts":drafts.ok(),"message":"Could not refresh this account; results may be incomplete."})),
                    }
                }
                Ok(
                    json!({"accounts":results,"checked_at":chrono::Utc::now(),"history_limit":limit,"complete":false}),
                )
            }
            "mail.search" => {
                let ids: Vec<Uuid> = serde_json::from_value(
                    params.get("account_ids").cloned().unwrap_or(json!([])),
                )?;
                let query = SearchQuery {
                    text: params
                        .get("query")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    account_ids: ids,
                    limit: Some(
                        params
                            .get("limit")
                            .and_then(Value::as_u64)
                            .unwrap_or(50)
                            .clamp(1, 500) as u32,
                    ),
                    ..Default::default()
                };
                let remote = params
                    .get("remote")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let messages = if remote {
                    super::search_local_and_remote(&self.store, &query).await?
                } else {
                    self.store.search(&query).await?
                };
                let incomplete = messages.len() >= query.limit.unwrap() as usize;
                Ok(
                    json!({"messages":messages.iter().map(message_for_agent).collect::<Result<Vec<_>>>()?,"incomplete":incomplete,"source":if remote {"local_and_remote"} else {"local_catalogue"}}),
                )
            }
            "mail.read" => {
                let id = string(&params, "message_id")?;
                let mut message = self.store.message(id).await?.context("message not found")?;
                let cached = if message.is_flagged {
                    if let Some((body_text, body_html)) = self.store.starred_body(id).await? {
                        Some(dakia_core::CachedMessageContent {
                            body_text,
                            body_html,
                            unsubscribe_kind: None,
                            attachments: self.store.starred_attachment_metadata(id).await?,
                        })
                    } else {
                        None
                    }
                } else {
                    self.store.cached_message_content(id).await?
                };
                let source;
                if let Some(cached) = cached {
                    message.body_text = cached.body_text;
                    message.body_html = cached.body_html;
                    message.attachments = cached
                        .attachments
                        .into_iter()
                        .map(|attachment| dakia_core::storage::AttachmentData {
                            attachment,
                            bytes: Vec::new(),
                        })
                        .collect();
                    source = "local_cache";
                } else if message.content_state == "complete" && !message.body_text.is_empty() {
                    source = "local_cache";
                } else {
                    message = super::fetch_message(&self.store, id).await?;
                    source = "provider";
                }
                Ok(json!({"message":message_for_agent(&message)?,"source":source}))
            }
            "mail.thread" => {
                let account_id = uuid(&params, "account_id")?;
                let message_id = string(&params, "message_id")?;
                let thread = self
                    .store
                    .conversation_for_target(&ConversationTarget {
                        account_id,
                        local_message_id: Some(message_id.to_owned()),
                        rfc_message_id: None,
                        thread_id: None,
                        mailbox: None,
                    })
                    .await?
                    .context("conversation not found in this account")?;
                Ok(
                    json!({"thread":{"account_id":account_id,"thread_id":thread.thread_id,"messages":thread.messages.iter().map(message_for_agent).collect::<Result<Vec<_>>>()?}}),
                )
            }
            "credential.acquire" => {
                if let Some((_, _, started)) = &self.credential_lease {
                    if started.elapsed() > Duration::from_secs(120) {
                        self.credential_lease = None;
                    }
                }
                if self.credential_lease.is_some() {
                    bail!("a credential update is already active");
                }
                let file = credential_lock_file(&self.profile)?;
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    match file.try_lock() {
                        Ok(()) => break,
                        Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                            tokio::time::sleep(Duration::from_millis(25)).await
                        }
                        Err(_) => bail!("provider credentials are busy; try again"),
                    }
                }
                let token = Uuid::new_v4().to_string();
                self.credential_lease = Some((token.clone(), file, Instant::now()));
                Ok(json!({"lease_token":token}))
            }
            "credential.release" => {
                let supplied = string(&params, "lease_token")?;
                if !self
                    .credential_lease
                    .as_ref()
                    .is_some_and(|(token, _, _)| token == supplied)
                {
                    bail!("credential update lease is missing or expired");
                }
                self.credential_lease = None;
                Ok(json!({"released":true}))
            }
            "credential.read" => {
                let provider = provider_id(&params)?;
                let values = self.credentials().await?;
                Ok(values.get(provider).cloned().unwrap_or(Value::Null))
            }
            "credential.list" => {
                let values = self.credentials().await?;
                Ok(json!(values.iter().map(|(id,v)| json!({"providerId":id,"type":v.get("type").and_then(Value::as_str).unwrap_or("unknown")})).collect::<Vec<_>>()))
            }
            "credential.write" | "credential.delete" => {
                self.validate_lease(&params)?;
                let provider = provider_id(&params)?.to_owned();
                let mut values = self.credentials().await?;
                if method == "credential.delete" {
                    values.remove(&provider);
                } else {
                    let value = params.get("credential").context("credential is required")?;
                    if !value.is_object() {
                        bail!("credential must be an object");
                    }
                    values.insert(provider, value.clone());
                }
                self.store
                    .set_secret(CREDENTIAL_KEY, &serde_json::to_string(&values)?)
                    .await?;
                Ok(json!({"saved":true}))
            }
            "action.list" => Ok(json!({"actions":self.store.agent_actions().await?})),
            "approval.grant" => self.grant_approval(&params),
            _ => self.mail_dispatch(method, params).await,
        }
    }

    fn validate_lease(&self, params: &Value) -> Result<()> {
        let supplied = string(params, "lease_token")?;
        if !self
            .credential_lease
            .as_ref()
            .is_some_and(|(token, _, started)| {
                token == supplied && started.elapsed() < Duration::from_secs(120)
            })
        {
            bail!("credential update lease is missing or expired");
        }
        Ok(())
    }

    async fn credentials(&self) -> Result<serde_json::Map<String, Value>> {
        match self.store.secret(CREDENTIAL_KEY).await? {
            Some(value) => Ok(serde_json::from_str(&value)
                .context("stored provider credentials could not be read")?),
            None => Ok(serde_json::Map::new()),
        }
    }

    async fn mail_dispatch(&mut self, method: &str, params: Value) -> Result<Value> {
        match method {
            "draft.list" => {
                let account = params
                    .get("account_id")
                    .and_then(Value::as_str)
                    .map(Uuid::parse_str)
                    .transpose()?;
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(25)
                    .clamp(1, 100) as u32;
                let offset = params
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(100_000) as u32;
                let drafts = self
                    .store
                    .agent_draft_summaries(account, limit, offset)
                    .await?;
                let next_offset = (drafts.len() == limit as usize).then_some(offset + limit);
                Ok(json!({"drafts":drafts,"next_offset":next_offset}))
            }
            "draft.get" => {
                let draft = self
                    .drafts()
                    .get(string(&params, "draft_id")?)
                    .await?
                    .context("draft not found")?;
                Ok(json!({"draft":draft_for_agent(&draft)?}))
            }
            "draft.delete" => {
                let deleted = self.drafts().delete(string(&params, "draft_id")?).await?;
                Ok(json!({"deleted":deleted}))
            }
            "draft.save" => {
                if params.get("draft_id").and_then(Value::as_str).is_some()
                    && params
                        .get("reply_to_message_id")
                        .and_then(Value::as_str)
                        .is_some()
                {
                    bail!("an existing draft keeps its reply target; omit reply_to_message_id when editing it");
                }
                let account_id = uuid(&params, "account_id")?;
                let account = self
                    .store
                    .account(account_id)
                    .await?
                    .context("account not found")?;
                if !account.enabled {
                    bail!("account is disabled");
                }
                let body = params
                    .get("body_text")
                    .and_then(Value::as_str)
                    .context("body_text is required")?
                    .to_owned();
                let mail = MailService::new(self.store.clone());
                let existing = if let Some(id) = params.get("draft_id").and_then(Value::as_str) {
                    Some(self.drafts().get(id).await?.context("draft not found")?)
                } else {
                    None
                };
                if existing
                    .as_ref()
                    .is_some_and(|d| d.account_id != account_id)
                {
                    bail!("draft belongs to another account");
                }
                let mut compose = if let Some(reply_id) =
                    params.get("reply_to_message_id").and_then(Value::as_str)
                {
                    let message = self
                        .store
                        .message(reply_id)
                        .await?
                        .context("reply target not found")?;
                    if message.account_id != account_id.to_string() {
                        bail!("reply target belongs to another account");
                    }
                    mail.reply_draft(&account, &message, body.clone())?
                } else if let Some(existing) = &existing {
                    existing.compose.clone()
                } else {
                    dakia_core::ComposeMessage {
                        account_id,
                        to: Vec::new(),
                        cc: Vec::new(),
                        bcc: Vec::new(),
                        subject: String::new(),
                        body_text: body.clone(),
                        body_html: None,
                        in_reply_to: None,
                        references: None,
                        attachments: Vec::new(),
                    }
                };
                compose.body_text = body;
                // Editing through the plain-text agent replaces a rich-text
                // alternative too, so recipients cannot see stale HTML.
                compose.body_html = None;
                compose.to = addresses(&params, "to")?;
                if params.get("cc").is_some() {
                    compose.cc = addresses(&params, "cc")?;
                }
                if params.get("bcc").is_some() {
                    compose.bcc = addresses(&params, "bcc")?;
                }
                compose.subject = params
                    .get("subject")
                    .and_then(Value::as_str)
                    .context("subject is required")?
                    .to_owned();
                if params.get("attachments").is_some() {
                    compose.attachments = self.compose_attachments(&params).await?;
                }
                let service = self.drafts();
                let draft = if let Some(id) = params.get("draft_id").and_then(Value::as_str) {
                    let revision = params
                        .get("expected_revision")
                        .and_then(Value::as_i64)
                        .context("expected_revision is required when updating a draft")?;
                    service.update(id, revision, compose).await?
                } else {
                    service.create(compose).await?
                };
                // Local content is immediately durable. Provider reconciliation
                // cannot hold up subsequent edits and is retried by explicit sync.
                let id = draft.id.clone();
                tokio::spawn(async move {
                    let _ = service.reconcile(&account, &id).await;
                });
                Ok(json!({"draft":draft_for_agent(&draft)?}))
            }
            "approval.prepare" => {
                let kind = string(&params, "kind")?;
                if kind == "delete_messages" {
                    let ids: Vec<String> = serde_json::from_value(
                        params
                            .get("message_ids")
                            .cloned()
                            .context("message_ids required")?,
                    )?;
                    let snapshot = self.deletion_snapshot(ids).await?;
                    let binding = json!({"kind":"delete_messages","messages":snapshot["targets"].as_array().context("invalid targets")?.iter().map(|t|json!({"message_id":t["message_id"],"account":t["account"]["email"],"folder":t["mailbox"],"subject":t["subject"],"from":t["from"]})).collect::<Vec<_>>()});
                    return self.prepare_approval("delete_messages", &snapshot, binding);
                }
                if kind != "send_draft" {
                    bail!("unsupported approval kind");
                }
                let id = string(&params, "draft_id")?;
                let draft = self.drafts().get(id).await?.context("draft not found")?;
                let account = self
                    .store
                    .account(draft.account_id)
                    .await?
                    .context("account not found")?;
                let mut binding = draft_binding(&draft)?;
                binding["from"] = json!(account.email);
                if matches!(
                    draft.status,
                    DraftSyncStatus::Sent | DraftSyncStatus::SendUncertain
                ) {
                    bail!("this draft was already attempted; check its delivery result before creating another reply");
                }
                let mut snapshot = draft_snapshot(&draft);
                snapshot["account"] = json!(account);
                self.prepare_approval(id, &snapshot, binding)
            }
            "action.execute" => {
                let token = params.get("approval").context("approval is required")?;
                let action_id = string(token, "action_id")?;
                let pending = self
                    .approvals
                    .get(action_id)
                    .context("approval not found")?
                    .snapshot
                    .clone();
                if pending.get("kind").and_then(Value::as_str) != Some("delete_messages") {
                    bail!("this action is not a message deletion");
                }
                let ids: Vec<String> = pending["targets"]
                    .as_array()
                    .context("invalid targets")?
                    .iter()
                    .map(|v| string(v, "message_id").map(str::to_owned))
                    .collect::<Result<_>>()?;
                let current = self.deletion_snapshot(ids).await?;
                let action_id = self.consume_approval("delete_messages", token, &current)?;
                let targets = current["targets"].as_array().context("invalid targets")?;
                let mut applied = Vec::new();
                let mut failures = Vec::new();
                for target in targets {
                    let id = string(target, "message_id")?;
                    let account: dakia_core::Account =
                        serde_json::from_value(target["account"].clone())?;
                    let operation_id = format!("{action_id}:{id}");
                    self.store
                        .agent_begin_action(
                            &operation_id,
                            account.id,
                            "delete_message",
                            &snapshot_hash(target)?,
                        )
                        .await?;
                    let mailbox = string(target, "mailbox")?;
                    let uid = target["uid"].as_i64().context("invalid UID")?;
                    let validity = target["uid_validity"]
                        .as_i64()
                        .context("invalid UIDVALIDITY")?;
                    let result = MailService::new(self.store.clone())
                        .apply_action_at_validity(
                            &account,
                            mailbox,
                            u32::try_from(uid)?,
                            dakia_core::MailboxAction::Delete,
                            validity,
                        )
                        .await;
                    match result {
                        Ok(_) => {
                            self.store.agent_finish_action(&operation_id,"completed",Some("deleted")).await?;
                            applied.push(id.to_owned());
                        }
                        Err(_) => failures.push(json!({"message_id":id,"status":"uncertain","message":"Deletion could not be confirmed. Refresh mail before trying again."})),
                    }
                }
                Ok(json!({"action_id":action_id,"applied_ids":applied,"failures":failures}))
            }
            "draft.send" => {
                let id = string(&params, "draft_id")?;
                let draft = self.drafts().get(id).await?.context("draft not found")?;
                let account = self
                    .store
                    .account(draft.account_id)
                    .await?
                    .context("account not found")?;
                if !account.enabled {
                    bail!("account is disabled");
                }
                let mut snapshot = draft_snapshot(&draft);
                snapshot["account"] = json!(account);
                let approval = params.get("approval").context("approval is required")?;
                let action_id = self.consume_approval(id, approval, &snapshot)?;
                let hash = snapshot_hash(&snapshot)?;
                self.store
                    .agent_begin_action(&action_id, account.id, "send_draft", &hash)
                    .await?;
                match self
                    .drafts()
                    .send_revision(&account, id, draft.revision)
                    .await
                {
                    Ok(response) => {
                        self.store
                            .agent_finish_action(&action_id, "sent", Some(&response))
                            .await?;
                        Ok(json!({"status":"sent","action_id":action_id,"response":response}))
                    }
                    Err(_) => {
                        let current = self.drafts().get(id).await?;
                        if current.as_ref().is_some_and(|d| {
                            !matches!(
                                d.status,
                                DraftSyncStatus::SendUncertain | DraftSyncStatus::Sent
                            )
                        }) {
                            self.store
                                .agent_finish_action(
                                    &action_id,
                                    "failed",
                                    Some("draft changed or send could not start"),
                                )
                                .await?;
                            return Ok(
                                json!({"status":"not_sent","action_id":action_id,"message":"The draft changed or sending could not start. Reload and approve the current draft."}),
                            );
                        }
                        // The durable ledger and draft both retain the barrier.
                        // Raw SMTP errors can include message content or provider
                        // data, so the agent receives only this actionable state.
                        Ok(
                            json!({"status":"uncertain","action_id":action_id,"message":"Delivery is uncertain. Check Sent before creating another reply. Dakia will not retry automatically."}),
                        )
                    }
                }
            }
            "mail.attachment.download" => {
                let mut value = self.download_attachment(&params).await?;
                if let Some(fields) = value.as_object_mut() {
                    fields.remove("agent_path");
                }
                Ok(value)
            }
            "context.get" | "context.set" | "context.history" | "context.restore" => {
                let kind = match params
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("agent_memory")
                {
                    "notes" => ThreadContextKind::Notes,
                    "agent_memory" | "agentMemory" => ThreadContextKind::AgentMemory,
                    _ => bail!("invalid context kind"),
                };
                let account_id = uuid(&params, "account_id")?;
                let target_id = string(&params, "target_id")?;
                let mut mutation_target = None;
                let locator = if params.get("target").and_then(Value::as_str)
                    == Some("conversation")
                {
                    ThreadContextLocator {
                        account_id,
                        conversation_id: Uuid::parse_str(target_id)?,
                    }
                } else {
                    let target = ConversationTarget {
                        account_id,
                        local_message_id: (params.get("target").and_then(Value::as_str)
                            == Some("message"))
                        .then(|| target_id.to_owned()),
                        thread_id: (params.get("target").and_then(Value::as_str) == Some("thread"))
                            .then(|| target_id.to_owned()),
                        rfc_message_id: None,
                        mailbox: None,
                    };
                    mutation_target = Some(target.clone());
                    self.store
                        .thread_context_conversation_for_target(&target)
                        .await?
                        .context("conversation not found in this account")?
                        .locator
                };
                if method == "context.history" {
                    let history = self.store.thread_context_history(locator, kind).await?;
                    return Ok(
                        json!({"history":history.into_iter().map(|r|json!({"id":r.id,"revision":r.revision,"body":r.content,"created_at":r.created_at,"action":r.action})).collect::<Vec<_>>()}),
                    );
                }
                let context = match method {
                    "context.get" => self.store.thread_context_get(locator, kind).await?,
                    "context.set" => {
                        let revision = params
                            .get("expected_revision")
                            .and_then(Value::as_u64)
                            .context(
                                "expected_revision is required; read context before editing",
                            )?;
                        // An empty body is valid and clears the document with history.
                        let body = params
                            .get("body")
                            .and_then(Value::as_str)
                            .context("body is required")?;
                        if let Some(target) = &mutation_target {
                            self.store
                                .thread_context_set_for_target(target, kind, body, revision)
                                .await?
                        } else {
                            self.store
                                .thread_context_set(locator, kind, body, revision)
                                .await?
                        }
                    }
                    "context.restore" => {
                        let revision = params
                            .get("expected_revision")
                            .and_then(Value::as_u64)
                            .context("expected_revision is required")?;
                        let historical_revision = uuid(&params, "revision")?;
                        if let Some(target) = &mutation_target {
                            self.store
                                .thread_context_restore_for_target(
                                    target,
                                    kind,
                                    historical_revision,
                                    revision,
                                )
                                .await?
                        } else {
                            self.store
                                .thread_context_restore(
                                    locator,
                                    kind,
                                    historical_revision,
                                    revision,
                                )
                                .await?
                        }
                    }
                    _ => unreachable!(),
                };
                Ok(
                    json!({"body":context.content,"revision":context.revision,"updated_at":context.updated_at,"conversation_id":context.locator.conversation_id,"requires_review":context.requires_review}),
                )
            }
            "mail.organize" => {
                let action = string(&params, "action")?;
                if !matches!(
                    action,
                    "archive"
                        | "spam"
                        | "not_spam"
                        | "trash"
                        | "move"
                        | "copy"
                        | "flag"
                        | "mark_read"
                        | "mark_unread"
                ) {
                    bail!("unsupported organization action");
                }
                let mut ids: Vec<String> = serde_json::from_value(
                    params
                        .get("message_ids")
                        .cloned()
                        .context("message_ids required")?,
                )?;
                if ids.is_empty() || ids.len() > 100 {
                    bail!("select between 1 and 100 messages");
                }
                ids.sort();
                ids.dedup();
                // Capture every immutable account/UID target before starting a mutation.
                let mut targets = Vec::new();
                for id in &ids {
                    let message = self.store.message(id).await?.context("message not found")?;
                    let account = self
                        .store
                        .account(Uuid::parse_str(&message.account_id)?)
                        .await?
                        .context("account not found")?;
                    if !account.enabled {
                        bail!("account is disabled");
                    }
                    let validity = self
                        .store
                        .mailbox_catalog_state(account.id, &message.mailbox)
                        .await?
                        .context("sync this mailbox before organizing its messages")?
                        .uid_validity;
                    targets.push((message, account, validity));
                }
                let mail = MailService::new(self.store.clone());
                let mut applied = Vec::new();
                let mut failures = Vec::new();
                for (message, account, validity) in targets {
                    let uid = u32::try_from(message.uid)?;
                    let result: Result<()> = async {
                        match action {
                            "flag" => {
                                let flag = params
                                    .get("flag")
                                    .and_then(Value::as_str)
                                    .unwrap_or("starred");
                                let value = match flag {
                                    "starred" | "flagged" => true,
                                    "unstarred" | "unflagged" => false,
                                    _ => bail!("unsupported flag"),
                                };
                                mail.set_flags_at_validity(
                                    &account,
                                    &message.mailbox,
                                    uid,
                                    &[dakia_core::MessageFlag::Flagged],
                                    value,
                                    validity,
                                )
                                .await?;
                            }
                            "mark_read" | "mark_unread" => {
                                let value = action == "mark_read";
                                mail.set_flags_at_validity(
                                    &account,
                                    &message.mailbox,
                                    uid,
                                    &[dakia_core::MessageFlag::Seen],
                                    value,
                                    validity,
                                )
                                .await?;
                            }
                            "move" | "copy" => {
                                let destination = string(&params, "destination_folder")?;
                                if action == "move" {
                                    mail.move_message_at_validity(
                                        &account,
                                        &message.mailbox,
                                        uid,
                                        destination,
                                        validity,
                                    )
                                    .await?;
                                } else {
                                    mail.copy_message_at_validity(
                                        &account,
                                        &message.mailbox,
                                        uid,
                                        destination,
                                        validity,
                                    )
                                    .await?;
                                }
                            }
                            _ => {
                                let action = match action {
                                    "archive" => dakia_core::MailboxAction::Archive,
                                    "spam" => dakia_core::MailboxAction::Spam,
                                    "not_spam" => dakia_core::MailboxAction::NotSpam,
                                    "trash" => dakia_core::MailboxAction::Trash,
                                    _ => unreachable!(),
                                };
                                mail.apply_action_at_validity(
                                    &account,
                                    &message.mailbox,
                                    uid,
                                    action,
                                    validity,
                                )
                                .await?;
                            }
                        }
                        Ok(())
                    }
                    .await;
                    match result {
                        Ok(()) => applied.push(message.id),
                        Err(e) => {
                            failures.push(json!({"message_id":message.id,"message":e.to_string()}))
                        }
                    }
                }
                Ok(json!({"applied_ids":applied,"failures":failures}))
            }
            _ => bail!("unknown agent method"),
        }
    }

    fn drafts(&self) -> DraftService {
        DraftService::new(self.store.clone(), MailService::new(self.store.clone()))
    }

    async fn deletion_snapshot(&self, mut ids: Vec<String>) -> Result<Value> {
        ids.sort();
        ids.dedup();
        if ids.is_empty() || ids.len() > 100 {
            bail!("select between 1 and 100 messages");
        }
        let mut targets = Vec::new();
        for id in ids {
            let message = self
                .store
                .message(&id)
                .await?
                .context("message not found")?;
            let account = self
                .store
                .account(Uuid::parse_str(&message.account_id)?)
                .await?
                .context("account not found")?;
            if !account.enabled {
                bail!("account is disabled");
            }
            let state = self
                .store
                .mailbox_catalog_state(account.id, &message.mailbox)
                .await?
                .context("sync this mailbox before requesting permanent deletion")?;
            targets.push(json!({"message_id":id,"account":account,"mailbox":message.mailbox,"uid":message.uid,"uid_validity":state.uid_validity,"rfc_message_id":message.message_id,"subject":message.subject,"from":message.from_address}));
        }
        Ok(json!({"kind":"delete_messages","targets":targets}))
    }

    async fn download_attachment(&mut self, params: &Value) -> Result<Value> {
        let message_id = string(params, "message_id")?;
        let attachment_id = string(params, "attachment_id")?;
        let message = self
            .store
            .message(message_id)
            .await?
            .context("message not found")?;
        let account = self
            .store
            .account(Uuid::parse_str(&message.account_id)?)
            .await?
            .context("account not found")?;
        let data = MailService::new(self.store.clone())
            .fetch_attachment(
                &account,
                &message.mailbox,
                u32::try_from(message.uid)?,
                attachment_id,
            )
            .await?;
        if data.bytes.len() > 25 * 1024 * 1024 {
            bail!("attachment exceeds the 25 MiB limit");
        }
        let metadata = json!({"id":attachment_id,"name":data.attachment.filename,"mime_type":data.attachment.mime_type,"size":data.bytes.len()});
        let handle = self.stage_attachment(ComposeAttachment {
            filename: data.attachment.filename,
            mime_type: data.attachment.mime_type,
            content_base64: STANDARD.encode(data.bytes),
        })?;
        let mut result = metadata;
        result["handle"] = json!(handle);
        Ok(result)
    }

    fn stage_attachment(&mut self, attachment: ComposeAttachment) -> Result<String> {
        const MAX_CACHE: usize = 128 * 1024 * 1024;
        self.staged_attachments
            .retain(|_, a| a.touched.elapsed() < Duration::from_secs(600));
        if attachment.content_base64.len() > 36 * 1024 * 1024 {
            bail!("attachment exceeds the size limit");
        }
        while self
            .staged_attachments
            .values()
            .map(|a| a.attachment.content_base64.len())
            .sum::<usize>()
            + attachment.content_base64.len()
            > MAX_CACHE
        {
            let oldest = self
                .staged_attachments
                .iter()
                .min_by_key(|(_, a)| a.touched)
                .map(|(id, _)| id.clone())
                .context("attachment cache is full")?;
            self.staged_attachments.remove(&oldest);
        }
        let handle = Uuid::new_v4().to_string();
        self.staged_attachments.insert(
            handle.clone(),
            StagedAttachment {
                attachment,
                touched: Instant::now(),
            },
        );
        Ok(handle)
    }

    async fn compose_attachments(&mut self, params: &Value) -> Result<Vec<ComposeAttachment>> {
        let Some(attachments) = params.get("attachments") else {
            return Ok(Vec::new());
        };
        let entries = attachments
            .as_array()
            .context("attachments must be a list")?;
        if entries.len() > 100 {
            bail!("too many attachments");
        }
        let mut result = Vec::new();
        let mut total = 0usize;
        for item in entries {
            let handle = if let Some(handle) = item.get("handle").and_then(Value::as_str) {
                handle.to_owned()
            } else if item.get("message_id").is_some() && item.get("attachment_id").is_some() {
                self.download_attachment(item).await?["handle"]
                    .as_str()
                    .context("attachment handle unavailable")?
                    .to_owned()
            } else {
                bail!("agent attachments require a Dakia download handle or message attachment reference");
            };
            let staged = self
                .staged_attachments
                .get_mut(&handle)
                .filter(|a| a.touched.elapsed() < Duration::from_secs(600))
                .context("attachment handle expired; download it again")?;
            staged.touched = Instant::now();
            let bytes = STANDARD.decode(&staged.attachment.content_base64)?;
            total += bytes.len();
            if bytes.len() > 25 * 1024 * 1024 || total > 50 * 1024 * 1024 {
                bail!("attachment size limit exceeded");
            }
            result.push(staged.attachment.clone());
        }
        Ok(result)
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{key} is required"))
}
fn snapshot_hash(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn addresses(params: &Value, key: &str) -> Result<Vec<String>> {
    let Some(values) = params.get(key) else {
        return Ok(Vec::new());
    };
    values
        .as_array()
        .context("recipients must be a list")?
        .iter()
        .map(|value| {
            let address = value
                .as_str()
                .or_else(|| value.get("email").and_then(Value::as_str))
                .context("recipient email is required")?;
            if address.contains(['\r', '\n', '\0']) {
                bail!("recipient contains invalid control characters");
            }
            if let Some(name) = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
            {
                if name.contains(['\r', '\n', '\0']) {
                    bail!("recipient name contains invalid control characters");
                }
                Ok(format!(
                    "\"{}\" <{}>",
                    name.replace('\\', "\\\\").replace('"', "\\\""),
                    address
                ))
            } else {
                Ok(address.to_owned())
            }
        })
        .collect()
}
fn draft_snapshot(draft: &LocalDraft) -> Value {
    json!({"draft_id":draft.id,"account_id":draft.account_id,"revision":draft.revision,"compose":draft.compose})
}
fn draft_binding(draft: &LocalDraft) -> Result<Value> {
    let attachments: Result<Vec<Value>> = draft.compose.attachments.iter().map(|a| {
        let bytes = STANDARD.decode(&a.content_base64)?;
        Ok(json!({"name":a.filename,"mime_type":a.mime_type,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}))
    }).collect();
    Ok(
        json!({"kind":"send_draft","draft_id":draft.id,"account_id":draft.account_id,"revision":draft.revision,
        "to":draft.compose.to,"cc":draft.compose.cc,"bcc":draft.compose.bcc,"subject":draft.compose.subject,
        "body_text":draft.compose.body_text,"body_html":draft.compose.body_html,"attachments":attachments?}),
    )
}
fn draft_for_agent(draft: &LocalDraft) -> Result<Value> {
    let mut value = serde_json::to_value(draft)?;
    let attachments: Result<Vec<_>> = draft.compose.attachments.iter().map(|a| {
        let bytes = STANDARD.decode(&a.content_base64)?;
        Ok(json!({"filename":a.filename,"mime_type":a.mime_type,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}))
    }).collect();
    value["compose"]["attachments"] = json!(attachments?);
    Ok(value)
}
fn message_for_agent(message: &dakia_core::MailSummary) -> Result<Value> {
    let mut value = serde_json::to_value(message)?;
    value["attachments"] = json!(message
        .attachments
        .iter()
        .map(|a| &a.attachment)
        .collect::<Vec<_>>());
    Ok(value)
}
fn uuid(value: &Value, key: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(string(value, key)?)?)
}
fn provider_id(params: &Value) -> Result<&str> {
    let id = string(params, "provider_id")?;
    if id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("invalid provider ID");
    }
    Ok(id)
}
fn credential_lock_file(profile: &Path) -> Result<File> {
    let directory = profile.join("agent");
    std::fs::create_dir_all(&directory)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        options.mode(0o600);
    }
    Ok(options.open(directory.join("credentials.lock"))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn draft_tool_results_keep_attachment_bytes_private_and_approval_includes_html() {
        let store = Store::in_memory().await.unwrap();
        let input: dakia_core::AccountDraft = serde_json::from_value(
            json!({"email":"fixture@example.test","display_name":"Fixture"}),
        )
        .unwrap();
        let account = input.into_account(dakia_core::provider::by_id("custom").unwrap());
        store.save_account(&account).await.unwrap();
        let secret_attachment = STANDARD.encode(b"PRIVATE ATTACHMENT FIXTURE");
        let draft = DraftService::new(store.clone(), MailService::new(store.clone()))
            .create(dakia_core::ComposeMessage {
                account_id: account.id,
                to: vec!["john@example.test".into()],
                cc: vec![],
                bcc: vec![],
                subject: "Today".into(),
                body_text: "Plain preview".into(),
                body_html: Some("<p>A different HTML preview</p>".into()),
                in_reply_to: None,
                references: None,
                attachments: vec![ComposeAttachment {
                    filename: "report.pdf".into(),
                    mime_type: "application/pdf".into(),
                    content_base64: secret_attachment.clone(),
                }],
            })
            .await
            .unwrap();
        let result = draft_for_agent(&draft).unwrap();
        let summaries = store
            .agent_draft_summaries(Some(account.id), 1, 0)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["id"], draft.id);
        assert_eq!(summaries[0]["attachmentCount"], 1);
        assert_eq!(summaries[0]["to"][0], "john@example.test");
        assert!(!serde_json::to_string(&summaries)
            .unwrap()
            .contains(&secret_attachment));
        assert!(summaries[0].get("compose").is_none());
        assert!(store
            .agent_draft_summaries(Some(account.id), 1, 1)
            .await
            .unwrap()
            .is_empty());
        assert!(!result.to_string().contains(&secret_attachment));
        assert!(result["compose"]["attachments"][0]
            .get("content_base64")
            .is_none());
        assert_eq!(
            result["compose"]["attachments"][0]["filename"],
            "report.pdf"
        );
        assert_eq!(
            draft_binding(&draft).unwrap()["body_html"],
            "<p>A different HTML preview</p>"
        );
    }
    #[tokio::test]
    async fn credentials_require_lease_and_list_never_exposes_keys() {
        let temp = tempfile::tempdir().unwrap();
        let mut bridge = Bridge {
            store: Store::open(temp.path().join("dakia.db")).await.unwrap(),
            profile: temp.path().to_owned(),
            credential_lease: None,
            approvals: HashMap::new(),
            staged_attachments: HashMap::new(),
        };
        let input = json!({"provider_id":"fixture","credential":{"type":"api_key","key":"private-fixture-key"}});
        assert!(bridge
            .dispatch("credential.write", input.clone())
            .await
            .is_err());
        let lease = bridge
            .dispatch("credential.acquire", json!({}))
            .await
            .unwrap();
        let mut input = input;
        input["lease_token"] = lease["lease_token"].clone();
        bridge
            .dispatch("credential.write", input.clone())
            .await
            .unwrap();
        let listed = bridge.dispatch("credential.list", json!({})).await.unwrap();
        assert_eq!(listed, json!([{"providerId":"fixture","type":"api_key"}]));
        bridge.credential_lease.as_mut().unwrap().2 = Instant::now() - Duration::from_secs(121);
        assert!(bridge
            .dispatch("credential.write", input.clone())
            .await
            .is_err());
        assert!(bridge
            .dispatch("credential.release", json!({"lease_token":"wrong-token"}))
            .await
            .is_err());
        bridge.dispatch("credential.release", lease).await.unwrap();
        assert!(bridge.credential_lease.is_none());
        let probe = credential_lock_file(temp.path()).unwrap();
        probe
            .try_lock()
            .expect("expired lease release must drop the OS lock");
        drop(probe);
        assert!(bridge.dispatch("credential.write", input).await.is_err());
        let bytes = std::fs::read(temp.path().join("dakia.db")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private-fixture-key"));
    }

    #[tokio::test]
    async fn sending_requires_exact_fresh_single_use_approval() {
        let temp = tempfile::tempdir().unwrap();
        let mut bridge = Bridge {
            store: Store::in_memory().await.unwrap(),
            profile: temp.path().to_owned(),
            credential_lease: None,
            approvals: HashMap::new(),
            staged_attachments: HashMap::new(),
        };
        let snapshot = json!({"account_id":"account-a","to":["john@example.test"],"body_text":"I cannot make it today.","attachments":[]});
        let token = bridge
            .prepare_approval("draft-a", &snapshot, Value::Null)
            .unwrap();
        assert!(bridge
            .consume_approval("draft-a", &token, &snapshot)
            .is_err());
        bridge.grant_approval(&token).unwrap();
        assert!(bridge
            .consume_approval("draft-b", &token, &snapshot)
            .is_err());
        let mut changed = snapshot.clone();
        changed["to"] = json!(["different@example.test"]);
        assert!(bridge
            .consume_approval("draft-a", &token, &changed)
            .is_err());
        changed = snapshot.clone();
        changed["attachments"] = json!([{"filename":"new.txt","content_base64":"dGVzdA=="}]);
        assert!(bridge
            .consume_approval("draft-a", &token, &changed)
            .is_err());
        let id = bridge
            .consume_approval("draft-a", &token, &snapshot)
            .unwrap();
        assert_eq!(id, token["action_id"].as_str().unwrap());
        assert!(bridge
            .consume_approval("draft-a", &token, &snapshot)
            .is_err());
        let token = bridge
            .prepare_approval("draft-a", &snapshot, Value::Null)
            .unwrap();
        bridge
            .approvals
            .get_mut(token["action_id"].as_str().unwrap())
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(bridge.grant_approval(&token).is_err());
    }

    #[tokio::test]
    async fn model_can_only_attach_bounded_ephemeral_download_handles() {
        let temp = tempfile::tempdir().unwrap();
        let mut bridge = Bridge {
            store: Store::in_memory().await.unwrap(),
            profile: temp.path().to_owned(),
            credential_lease: None,
            approvals: HashMap::new(),
            staged_attachments: HashMap::new(),
        };
        assert!(bridge
            .compose_attachments(&json!({"attachments":[{"path":"/private/secret.txt"}]}))
            .await
            .is_err());
        let handle = bridge
            .stage_attachment(ComposeAttachment {
                filename: "report.txt".into(),
                mime_type: "text/plain".into(),
                content_base64: STANDARD.encode(b"downloaded fixture"),
            })
            .unwrap();
        let attachments = bridge
            .compose_attachments(&json!({"attachments":[{"handle":handle}]}))
            .await
            .unwrap();
        assert_eq!(attachments[0].filename, "report.txt");
        assert_eq!(
            STANDARD.decode(&attachments[0].content_base64).unwrap(),
            b"downloaded fixture"
        );
        bridge.staged_attachments.get_mut(&handle).unwrap().touched =
            Instant::now() - Duration::from_secs(601);
        assert!(bridge
            .compose_attachments(&json!({"attachments":[{"handle":handle}]}))
            .await
            .is_err());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
