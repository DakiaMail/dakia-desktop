#[cfg(test)]
use crate::storage::ThreadingHeaders;
use crate::{
    oauth::OAuthTokens,
    provider::Security,
    storage::{stable_message_id, Attachment, AttachmentData, MailSummary},
    Account, AccountAuth, Store,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use lettre::{
    address::Envelope,
    message::{
        header::ContentType, Attachment as LettreAttachment, Mailbox, MultiPart, SinglePart,
    },
    transport::smtp::authentication::{Credentials, Mechanism},
    Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use mail_auth::{AuthenticatedMessage, DkimResult, MessageAuthenticator};
use mailparse::MailHeaderMap;
use percent_encoding::percent_decode_str;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::watch,
    time::{timeout, Duration},
};
use tokio_rustls::{
    client::TlsStream,
    rustls::{pki_types::ServerName, ClientConfig, RootCertStore},
    TlsConnector,
};
use url::{Host, Url};

const IMAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IMAP_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const SMTP_SEND_TIMEOUT: Duration = Duration::from_secs(60);
const DKIM_VERIFY_TIMEOUT: Duration = Duration::from_secs(5);
const UNSUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_ATTACHMENT_COUNT: usize = 50;
const MAX_OUTBOUND_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_OUTBOUND_ATTACHMENT_TOTAL_BYTES: usize = 50 * 1024 * 1024;
/// Existing databases are enriched gradually during ordinary catalogue syncs.
/// The durable scanned bit makes a provider's truthful empty headers terminal.
const RECIPIENT_HEADER_BACKFILL_BATCH: u32 = 50;
const GMAIL_CATEGORY_SIGNAL_PREFIX: &str = "Gmail category: ";
// IDLE is the fast path, but providers and network intermediaries can delay or
// lose EXISTS notifications while leaving the connection open. Reconcile the
// durable UID watermark frequently enough to preserve the near-real-time UX.
const IMAP_IDLE_RENEWAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposeMessage {
    pub account_id: uuid::Uuid,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<String>,
    #[serde(default)]
    pub attachments: Vec<ComposeAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposeAttachment {
    pub filename: String,
    pub mime_type: String,
    pub content_base64: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailboxAction {
    Archive,
    Spam,
    NotSpam,
    Trash,
    Delete,
}

/// Maps a catalogue mailbox to the provider mailbox that owns it.  Keeping
/// this in the core crate prevents the desktop and command-line clients from
/// drifting when a provider uses names such as Gmail's system folders.
pub fn remote_mailbox(account: &Account, mailbox: &str) -> String {
    if let Some((_, remote)) = mailbox.split_once("::") {
        return remote.to_owned();
    }
    match mailbox {
        "Archive" => account.archive_mailbox.clone(),
        "Spam" => account.spam_mailbox.clone(),
        "Sent" if account.provider_id == "gmail" => "[Gmail]/Sent Mail".into(),
        "Drafts" if account.provider_id == "gmail" => "[Gmail]/Drafts".into(),
        "Trash" if account.provider_id == "gmail" => "[Gmail]/Trash".into(),
        _ => mailbox.to_owned(),
    }
}

/// Returns the local catalogue folder for a move action.  Permanent deletion
/// intentionally has no destination because its catalogue row must disappear.
pub fn mailbox_action_destination(action: MailboxAction) -> Option<&'static str> {
    match action {
        MailboxAction::Archive => Some("Archive"),
        MailboxAction::Spam => Some("Spam"),
        MailboxAction::NotSpam => Some("INBOX"),
        MailboxAction::Trash => Some("Trash"),
        MailboxAction::Delete => None,
    }
}

#[derive(Debug, Clone)]
pub enum UnsubscribeOutcome {
    Completed,
    Web(String),
    Mailto {
        to: String,
        subject: String,
        body: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    pub phase: &'static str,
    pub completed: usize,
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub synced_count: usize,
    pub new_messages: Vec<MailSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeMode {
    Idle,
    Poll,
}

#[derive(Debug, Clone)]
pub struct RealtimeCycle {
    pub mode: RealtimeMode,
    pub new_messages: Vec<MailSummary>,
    pub cancelled: bool,
    pub detected_at: Option<chrono::DateTime<Utc>>,
    pub pending_hydration: Vec<MailSummary>,
}

#[derive(Clone)]
pub struct CredentialStore {
    store: Store,
    service: String,
}

impl CredentialStore {
    fn new(store: Store) -> Self {
        Self {
            store,
            service: "dev.dakia.mail".into(),
        }
    }

    fn key(&self, account: &Account) -> String {
        format!("{}:{}", account.id, account.auth.username())
    }
    pub async fn set_password(&self, account: &Account, password: &str) -> Result<()> {
        self.store_secret(account, password).await
    }
    pub async fn set_oauth_tokens(&self, account: &Account, tokens: &OAuthTokens) -> Result<()> {
        self.store_secret(account, &serde_json::to_string(tokens)?)
            .await
    }
    pub async fn delete(&self, account: &Account) -> Result<()> {
        self.store
            .delete_secret(&format!("{}:{}", self.service, self.key(account)))
            .await
    }
    async fn secret(&self, account: &Account) -> Result<String> {
        if let Ok(value) = std::env::var(format!(
            "DAKIA_PASSWORD_{}",
            account.id.to_string().replace('-', "_").to_uppercase()
        )) {
            return Ok(value);
        }
        let stored = self.load_secret(account).await?;
        if matches!(account.auth, AccountAuth::OAuth2 { .. }) {
            let mut tokens: OAuthTokens =
                serde_json::from_str(&stored).context("stored OAuth credentials are invalid")?;
            if tokens.should_refresh() {
                tokens.refresh().await?;
                self.store_secret(account, &serde_json::to_string(&tokens)?)
                    .await?;
            }
            Ok(tokens.access_token)
        } else {
            Ok(stored)
        }
    }

    async fn store_secret(&self, account: &Account, secret: &str) -> Result<()> {
        self.store
            .set_secret(&format!("{}:{}", self.service, self.key(account)), secret)
            .await
    }

    async fn load_secret(&self, account: &Account) -> Result<String> {
        self.store
            .secret(&format!("{}:{}", self.service, self.key(account)))
            .await?
            .context("credentials are not stored for this account")
    }
}

#[derive(Clone)]
pub struct MailService {
    store: Store,
    credentials: CredentialStore,
    dkim_authenticator: Option<MessageAuthenticator>,
}

impl MailService {
    pub fn new(store: Store) -> Self {
        Self {
            credentials: CredentialStore::new(store.clone()),
            store,
            dkim_authenticator: MessageAuthenticator::new_system_conf()
                .or_else(|_| MessageAuthenticator::new_cloudflare_tls())
                .ok(),
        }
    }
    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    pub async fn sync_inbox(&self, account: &Account, max_messages: u32) -> Result<usize> {
        Ok(self
            .sync_inbox_with_progress(account, max_messages, |_| {})
            .await?
            .synced_count)
    }

    pub async fn sync_all(&self, account: &Account, max_messages: u32) -> Result<usize> {
        Ok(self
            .sync_all_with_progress(account, max_messages, |_| {})
            .await?
            .synced_count)
    }

    /// Opens a short-lived supervised INBOX session. It first reconciles the
    /// complete remote UID set and fetches new UIDs after the durable
    /// watermark, then waits in IMAP IDLE when the server advertises it. The
    /// caller loops cycles and owns reconnect policy.
    pub async fn realtime_inbox_cycle(
        &self,
        account: &Account,
        max_messages: u32,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<RealtimeCycle> {
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client
            .authenticate(account, &secret)
            .await
            .context("IMAP authentication failed")?;
        let capabilities = client.command("CAPABILITY").await?;
        let supports_idle = supports_idle(&capabilities);
        let select = client.command("SELECT INBOX").await?;
        let uid_validity = parse_uid_validity(&select);
        let mut detected_at = Some(Utc::now());
        let mut new_messages = self
            .fetch_new_inbox_headers(
                &mut client,
                account,
                max_messages,
                uid_validity.map(u64::from),
            )
            .await?;
        if new_messages.is_empty() && supports_idle {
            match client.idle_until_change(cancel).await? {
                IdleOutcome::Changed => {
                    detected_at = Some(Utc::now());
                    new_messages = self
                        .fetch_new_inbox_headers(
                            &mut client,
                            account,
                            max_messages,
                            uid_validity.map(u64::from),
                        )
                        .await?;
                }
                IdleOutcome::Cancelled => {
                    return Ok(RealtimeCycle {
                        mode: RealtimeMode::Idle,
                        new_messages: Vec::new(),
                        cancelled: true,
                        detected_at: None,
                        pending_hydration: Vec::new(),
                    });
                }
                IdleOutcome::Renewed => {
                    detected_at = Some(Utc::now());
                    new_messages = self
                        .fetch_new_inbox_headers(
                            &mut client,
                            account,
                            max_messages,
                            uid_validity.map(u64::from),
                        )
                        .await?;
                }
            }
        }
        let _ = client.command("LOGOUT").await;
        let detected_at = (!new_messages.is_empty()).then_some(detected_at).flatten();
        // Historical header-only rows are hydrated on demand. Feeding that
        // backlog into the real-time watcher can keep it away from IMAP IDLE
        // for minutes and delay later arrivals. Only the messages detected by
        // this cycle are candidates for opportunistic background hydration.
        let pending_hydration = new_messages.clone();
        Ok(RealtimeCycle {
            mode: if supports_idle {
                RealtimeMode::Idle
            } else {
                RealtimeMode::Poll
            },
            new_messages,
            cancelled: false,
            detected_at,
            pending_hydration,
        })
    }

    async fn fetch_new_inbox_headers(
        &self,
        client: &mut ImapClient,
        account: &Account,
        max_messages: u32,
        uid_validity: Option<u64>,
    ) -> Result<Vec<MailSummary>> {
        let state = self
            .store
            .prepare_mailbox_sync(account.id, "INBOX", uid_validity)
            .await?;
        // Realtime sync must observe removals as well as arrivals. Searching
        // only above the durable watermark leaves an archived UID in the
        // local Inbox forever: the row can reappear after the optimistic UI
        // update, but hydrating it then fails because the provider no longer
        // has that UID in INBOX.
        let search = client.command("UID SEARCH ALL").await?;
        let remote_uids = parse_search_uids(&search);
        let remote_set = remote_uids.iter().copied().collect();
        self.store
            .reconcile_mailbox_uids(account.id, "INBOX", &remote_set)
            .await?;
        let mut uids = sync_uids(remote_uids, state.highest_uid, max_messages);
        let mut scheduled = uids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        for uid in self
            .store
            .unscanned_recipient_header_uids(account.id, "INBOX", RECIPIENT_HEADER_BACKFILL_BATCH)
            .await?
        {
            if remote_set.contains(&uid) && scheduled.insert(uid) {
                uids.push(uid);
            }
        }
        let mut messages = Vec::with_capacity(uids.len());
        for uid in uids {
            let fields = "FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE LIST-ID PRECEDENCE AUTO-SUBMITTED)]";
            let response = client
                .command_with_literal(&format!("UID FETCH {uid} ({fields})"))
                .await?;
            if let Some(raw) = response.literal {
                messages.push(parse_header_message(
                    account,
                    "INBOX",
                    uid,
                    &response.lines,
                    &raw,
                )?);
            }
        }
        let new_messages = self
            .store
            .save_synced_messages(account.id, "INBOX", &messages)
            .await?;
        self.store
            .set_mailbox_uid_validity(account.id, "INBOX", uid_validity)
            .await?;
        Ok(new_messages)
    }

    pub async fn hydrate_inbox_message(&self, account: &Account, uid: u32) -> Result<MailSummary> {
        self.hydrate_message(account, "INBOX", uid).await
    }

    pub async fn hydrate_message(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
    ) -> Result<MailSummary> {
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        client
            .command(&format!(
                "SELECT {}",
                quote_imap(&remote_mailbox(account, mailbox))
            ))
            .await?;
        let response = client
            .command_with_literal(&hydration_fetch_command(account, uid))
            .await?;
        let raw = response
            .literal
            .context("IMAP server did not return the requested message")?;
        let message = parse_message(
            account,
            mailbox,
            uid,
            &response.lines,
            &raw,
            self.dkim_authenticator.as_ref(),
        )
        .await?;
        self.store
            .upsert_messages(std::slice::from_ref(&message))
            .await?;
        let _ = client.command("LOGOUT").await;
        Ok(message)
    }

    pub async fn set_flagged(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        flagged: bool,
    ) -> Result<()> {
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        client
            .command(&format!(
                "SELECT {}",
                quote_imap(&remote_mailbox(account, mailbox))
            ))
            .await?;
        let operation = if flagged {
            "+FLAGS.SILENT"
        } else {
            "-FLAGS.SILENT"
        };
        client
            .command(&format!("UID STORE {uid} {operation} (\\Flagged)"))
            .await?;
        let _ = client.command("LOGOUT").await;
        Ok(())
    }

    pub async fn set_read(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        read: bool,
    ) -> Result<()> {
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        client
            .command(&format!(
                "SELECT {}",
                quote_imap(&remote_mailbox(account, mailbox))
            ))
            .await?;
        client.command(&set_read_command(uid, read)).await?;
        let _ = client.command("LOGOUT").await;
        Ok(())
    }

    pub async fn sync_inbox_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        self.sync_mailboxes_with_progress(
            account,
            max_messages,
            vec![MailboxPlan::new("INBOX", "INBOX")],
            false,
            on_progress,
        )
        .await
    }

    pub async fn sync_all_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        self.sync_mailboxes_with_progress(
            account,
            max_messages,
            mailbox_plans(account),
            false,
            on_progress,
        )
        .await
    }

    pub async fn rebuild_all_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        self.sync_mailboxes_with_progress(
            account,
            max_messages,
            mailbox_plans(account),
            true,
            on_progress,
        )
        .await
    }

    /// Continues a rebuild after the process was interrupted. Catalogue state
    /// and committed message batches describe the remaining provider work, so
    /// resuming must not clear the partial index again.
    pub async fn resume_rebuild_all_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        self.sync_mailboxes_with_progress(
            account,
            max_messages,
            mailbox_plans(account),
            false,
            on_progress,
        )
        .await
    }

    pub async fn refresh_inbox_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        mut on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        on_progress(SyncProgress {
            phase: "connecting",
            completed: 0,
            total: None,
        });
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        on_progress(SyncProgress {
            phase: "authenticating",
            completed: 0,
            total: None,
        });
        client.authenticate(account, &secret).await?;
        on_progress(SyncProgress {
            phase: "finding",
            completed: 0,
            total: None,
        });
        let select = client.command("SELECT INBOX").await?;
        let uid_validity = parse_uid_validity(&select).map(u64::from);
        let before = self.store.mailbox_uids(account.id, "INBOX").await?;
        let new_messages = self
            .fetch_new_inbox_headers(&mut client, account, max_messages, uid_validity)
            .await?;
        let after = self.store.mailbox_uids(account.id, "INBOX").await?;
        let _ = client.command("LOGOUT").await;
        let synced_count = after.difference(&before).count();
        on_progress(SyncProgress {
            phase: "saving",
            completed: synced_count,
            total: Some(synced_count),
        });
        on_progress(SyncProgress {
            phase: "complete",
            completed: synced_count,
            total: Some(synced_count),
        });
        Ok(SyncResult {
            synced_count,
            new_messages,
        })
    }

    async fn sync_mailboxes_with_progress<F>(
        &self,
        account: &Account,
        max_messages: u32,
        plans: Vec<MailboxPlan>,
        reset_before_sync: bool,
        mut on_progress: F,
    ) -> Result<SyncResult>
    where
        F: FnMut(SyncProgress),
    {
        on_progress(SyncProgress {
            phase: "connecting",
            completed: 0,
            total: None,
        });
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        on_progress(SyncProgress {
            phase: "authenticating",
            completed: 0,
            total: None,
        });
        client.authenticate(account, &secret).await?;
        let listing = client.command("LIST \"\" \"*\"").await.unwrap_or_default();
        let plans = resolve_special_mailboxes(plans, &listing);
        if reset_before_sync {
            client
                .command("SELECT INBOX")
                .await
                .context("cannot rebuild because the provider inbox is unavailable")?;
            self.store.reset_account_mail_index(account.id).await?;
        }
        on_progress(SyncProgress {
            phase: "finding",
            completed: 0,
            total: None,
        });
        let mut work = Vec::new();
        for plan in plans {
            let selected = match client
                .command(&format!("SELECT {}", quote_imap(&plan.remote)))
                .await
            {
                Ok(selected) => selected,
                Err(_) => {
                    if plan.local == "INBOX" {
                        bail!("IMAP server does not expose an inbox");
                    }
                    continue;
                }
            };
            let uid_validity = parse_uid_validity(&selected)
                .context("IMAP server omitted UIDVALIDITY after SELECT")?;
            let previous_state = self
                .store
                .mailbox_catalog_state(account.id, &plan.storage)
                .await?;
            if previous_state
                .as_ref()
                .is_some_and(|state| state.uid_validity != i64::from(uid_validity))
            {
                self.store
                    .reset_mailbox_catalog(account.id, &plan.storage)
                    .await?;
            }
            let search = client.command("UID SEARCH ALL").await?;
            let mut remote_uids = parse_search_uids(&search);
            remote_uids.sort_unstable();
            let current_flags = client.command("UID FETCH 1:* (UID FLAGS)").await?;
            self.store
                .update_mailbox_flags(account.id, &plan.storage, &parse_uid_flags(&current_flags))
                .await?;
            let remote_set = remote_uids.iter().copied().collect();
            self.store
                .reconcile_mailbox_uids(account.id, &plan.storage, &remote_set)
                .await?;
            let local_uids = self.store.mailbox_uids(account.id, &plan.storage).await?;
            let previous_highest = local_uids.iter().copied().max();
            let mut missing = missing_uids_newest_first(&remote_uids, &local_uids);
            let mut scheduled: std::collections::HashSet<_> = missing.iter().copied().collect();
            for uid in self
                .store
                .mime_encoded_snippet_uids(account.id, &plan.storage)
                .await?
            {
                if remote_set.contains(&uid) && scheduled.insert(uid) {
                    missing.push(uid);
                }
            }
            for uid in self
                .store
                .unscanned_recipient_header_uids(
                    account.id,
                    &plan.storage,
                    RECIPIENT_HEADER_BACKFILL_BATCH,
                )
                .await?
            {
                if remote_set.contains(&uid) && scheduled.insert(uid) {
                    missing.push(uid);
                }
            }
            // Newest-first means the useful end of every mailbox appears
            // immediately. Every committed batch is resumable because the
            // remaining work is derived from the catalogue on reconnect.
            self.store
                .save_mailbox_catalog_state(
                    account.id,
                    &plan.storage,
                    &plan.remote,
                    uid_validity,
                    remote_uids.len(),
                    missing.is_empty(),
                )
                .await?;
            work.push(MailboxSyncWork {
                plan,
                uid_validity,
                remote_total: remote_uids.len(),
                initialized: previous_state.is_some(),
                previous_highest,
                uids: missing,
                offset: 0,
            });
        }
        let total = work.iter().map(|item| item.uids.len()).sum();
        on_progress(SyncProgress {
            phase: "downloading",
            completed: 0,
            total: Some(total),
        });
        let mut completed = 0;
        let mut synced = 0;
        let mut new_messages = Vec::new();
        let batch_size = max_messages.clamp(1, 50) as usize;
        loop {
            let mut published = false;
            for item in &mut work {
                if item.offset >= item.uids.len() {
                    continue;
                }
                published = true;
                client
                    .command(&format!("SELECT {}", quote_imap(&item.plan.remote)))
                    .await?;
                let end = (item.offset + batch_size).min(item.uids.len());
                let batch = &item.uids[item.offset..end];
                let mut messages = Vec::with_capacity(batch.len());
                for uid in batch {
                    let fields = if account.provider_id == "gmail" {
                        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE X-GM-LABELS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
                    } else {
                        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
                    };
                    let response = client
                        .command_with_literal(&format!("UID FETCH {uid} ({fields})"))
                        .await?;
                    if let Some(headers) = response.literal {
                        if !item.plan.skip_gmail_system_labels
                            || gmail_all_mail_is_archive(&response.lines)
                        {
                            let snippet = client
                                .command_with_literal(&format!(
                                    "UID FETCH {uid} (BODY.PEEK[]<0.8192>)"
                                ))
                                .await
                                .ok()
                                .and_then(|response| response.literal)
                                .map(|raw| snippet_from_partial(&raw))
                                .unwrap_or_default();
                            messages.push(parse_catalog_message(
                                account,
                                &item.plan.storage,
                                *uid,
                                &response.lines,
                                &headers,
                                snippet,
                            )?);
                        }
                    }
                    completed += 1;
                    on_progress(SyncProgress {
                        phase: "downloading",
                        completed,
                        total: Some(total),
                    });
                }
                self.store.upsert_catalog_messages(&messages).await?;
                if item.initialized && item.plan.local == "INBOX" {
                    new_messages.extend(
                        messages
                            .iter()
                            .filter(|message| {
                                !message.is_read
                                    && item
                                        .previous_highest
                                        .is_some_and(|highest| message.uid > i64::from(highest))
                            })
                            .cloned(),
                    );
                }
                synced += messages.len();
                on_progress(SyncProgress {
                    phase: "saving",
                    completed,
                    total: Some(total),
                });
                on_progress(SyncProgress {
                    phase: "downloading",
                    completed,
                    total: Some(total),
                });
                item.offset = end;
                if item.offset == item.uids.len() {
                    self.store
                        .save_mailbox_catalog_state(
                            account.id,
                            &item.plan.storage,
                            &item.plan.remote,
                            item.uid_validity,
                            item.remote_total,
                            true,
                        )
                        .await?;
                }
            }
            if !published {
                break;
            }
        }
        if synced > 0 {
            self.store.finish_threading_backfill(account.id).await?;
        }
        let _ = client.command("LOGOUT").await;
        on_progress(SyncProgress {
            phase: "saving",
            completed: synced,
            total: Some(total),
        });
        on_progress(SyncProgress {
            phase: "complete",
            completed: synced,
            total: Some(total),
        });
        Ok(SyncResult {
            synced_count: synced,
            new_messages,
        })
    }

    /// Fetches a complete message only for an explicit foreground action.
    /// The returned body and attachment bytes are transient and are never
    /// written back to the catalogue.
    pub async fn fetch_message(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
    ) -> Result<MailSummary> {
        let state = self
            .store
            .mailbox_catalog_state(account.id, mailbox)
            .await?
            .context("mailbox catalogue is not initialized")?;
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        let selected = client
            .command(&format!("SELECT {}", quote_imap(&state.remote_name)))
            .await?;
        let current_uid_validity = parse_uid_validity(&selected)
            .context("IMAP server omitted UIDVALIDITY after SELECT")?;
        if i64::from(current_uid_validity) != state.uid_validity {
            bail!("mailbox identity changed; sync the account before opening this message");
        }
        let fields = if account.provider_id == "gmail" {
            "FLAGS INTERNALDATE X-GM-LABELS RFC822"
        } else {
            "FLAGS INTERNALDATE RFC822"
        };
        let response = client
            .command_with_literal(&format!("UID FETCH {uid} ({fields})"))
            .await?;
        let raw = response.literal.context("message is no longer available")?;
        let message = parse_message(
            account,
            mailbox,
            uid,
            &response.lines,
            &raw,
            self.dkim_authenticator.as_ref(),
        )
        .await?;
        let _ = client.command("LOGOUT").await;
        Ok(message)
    }

    /// Searches message bodies on the authoritative server. Results are
    /// locators only; callers merge catalogue rows with the immediate local
    /// FTS results and retain those local results if the server is offline.
    pub async fn search_remote(
        &self,
        account: &Account,
        text: &str,
        mailbox: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailSummary>> {
        if text.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        let listing = client.command("LIST \"\" \"*\"").await.unwrap_or_default();
        let plans = resolve_special_mailboxes(mailbox_plans(account), &listing);
        let mut hits = Vec::new();
        for plan in plans {
            if mailbox.is_some_and(|requested| requested != plan.local) {
                continue;
            }
            if client
                .command(&format!("SELECT {}", quote_imap(&plan.remote)))
                .await
                .is_err()
            {
                continue;
            }
            let text_criterion = format!("TEXT {}", quote_imap(text.trim()));
            let response = if account.provider_id == "gmail" {
                let gmail = format!("X-GM-RAW {}", quote_imap(text.trim()));
                match client.command(&format!("UID SEARCH {gmail}")).await {
                    Ok(response) => response,
                    Err(_) => {
                        client
                            .command(&format!("UID SEARCH {text_criterion}"))
                            .await?
                    }
                }
            } else {
                client
                    .command(&format!("UID SEARCH {text_criterion}"))
                    .await?
            };
            let mut uids = parse_search_uids(&response);
            uids.sort_unstable_by(|left, right| right.cmp(left));
            for uid in uids {
                if let Some(message) = self
                    .store
                    .message_by_locator(account.id, &plan.storage, uid)
                    .await?
                {
                    hits.push(message);
                } else {
                    // Search can outrun a long historical backfill. Publish
                    // metadata for an online-only hit immediately without
                    // retaining its body.
                    let fields = if account.provider_id == "gmail" {
                        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE X-GM-LABELS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
                    } else {
                        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
                    };
                    let response = client
                        .command_with_literal(&format!("UID FETCH {uid} ({fields})"))
                        .await?;
                    if let Some(headers) = response.literal {
                        if plan.skip_gmail_system_labels
                            && !gmail_all_mail_is_archive(&response.lines)
                        {
                            continue;
                        }
                        let snippet = client
                            .command_with_literal(&format!("UID FETCH {uid} (BODY.PEEK[]<0.8192>)"))
                            .await
                            .ok()
                            .and_then(|response| response.literal)
                            .map(|raw| snippet_from_partial(&raw))
                            .unwrap_or_default();
                        let message = parse_catalog_message(
                            account,
                            &plan.storage,
                            uid,
                            &response.lines,
                            &headers,
                            snippet,
                        )?;
                        self.store
                            .upsert_catalog_messages(std::slice::from_ref(&message))
                            .await?;
                        hits.push(message);
                    }
                }
                if hits.len() >= limit {
                    break;
                }
            }
            if hits.len() >= limit {
                break;
            }
        }
        let _ = client.command("LOGOUT").await;
        Ok(hits)
    }

    /// Refreshes only Gmail's built-in category metadata for already-synced
    /// messages. It never downloads bodies, alters message categories, or
    /// resets the normal incremental-sync watermark.
    pub async fn refresh_gmail_category_metadata(&self, account: &Account) -> Result<usize> {
        if account.provider_id != "gmail" {
            bail!("Gmail category metadata is only available for Gmail accounts");
        }
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        let mut updates = Vec::new();
        for plan in mailbox_plans(account) {
            if client
                .command(&format!("SELECT {}", quote_imap(&plan.remote)))
                .await
                .is_err()
            {
                continue;
            }
            for row in self
                .store
                .mailbox_signal_metadata(account.id, plan.local)
                .await?
            {
                let lines = client
                    .command(&format!("UID FETCH {} (X-GM-LABELS)", row.uid))
                    .await?;
                let signals = merge_gmail_category_signal(
                    &row.classification_signals,
                    gmail_category_signal(&lines),
                );
                if signals != row.classification_signals {
                    updates.push((row.id, signals));
                }
            }
        }
        let refreshed = updates.len();
        self.store.update_classification_signals(&updates).await?;
        let _ = client.command("LOGOUT").await;
        Ok(refreshed)
    }

    pub async fn apply_action(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        action: MailboxAction,
    ) -> Result<Option<u32>> {
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        client
            .command(&format!("SELECT {}", quote_imap(mailbox)))
            .await?;
        let destination = match action {
            MailboxAction::Archive => &account.archive_mailbox,
            MailboxAction::Spam => &account.spam_mailbox,
            MailboxAction::NotSpam => "INBOX",
            MailboxAction::Trash => {
                if account.provider_id == "gmail" {
                    "[Gmail]/Trash"
                } else {
                    "Trash"
                }
            }
            MailboxAction::Delete => {
                client
                    .command(&format!("UID STORE {uid} +FLAGS.SILENT (\\Deleted)"))
                    .await?;
                client.command("EXPUNGE").await?;
                let _ = client.command("LOGOUT").await;
                return Ok(None);
            }
        };
        let response = client
            .command(&format!("UID MOVE {uid} {}", quote_imap(destination)))
            .await;
        let destination_uid = if let Ok(lines) = response {
            parse_copy_uid(&lines)
        } else {
            let lines = client
                .command(&format!("UID COPY {uid} {}", quote_imap(destination)))
                .await?;
            client
                .command(&format!("UID STORE {uid} +FLAGS.SILENT (\\Deleted)"))
                .await?;
            client.command("EXPUNGE").await?;
            parse_copy_uid(&lines)
        };
        let _ = client.command("LOGOUT").await;
        Ok(destination_uid)
    }

    pub async fn unsubscribe(&self, message: &MailSummary) -> Result<UnsubscribeOutcome> {
        let Some(kind) = &message.unsubscribe_kind else {
            bail!("This message does not provide an unsubscribe action");
        };
        let Some(value) = &message.unsubscribe_url else {
            bail!("This message has an invalid unsubscribe action");
        };
        let url = Url::parse(value).context("invalid unsubscribe URL")?;
        match kind.as_str() {
            "one_click" => {
                post_one_click_unsubscribe(&url).await?;
                Ok(UnsubscribeOutcome::Completed)
            }
            "web" if is_safe_web_url(&url) => Ok(UnsubscribeOutcome::Web(url.to_string())),
            "web" => bail!("unsubscribe page uses an unsafe URL"),
            "mailto" => {
                let action = parse_mailto_action(&url)?;
                Ok(UnsubscribeOutcome::Mailto {
                    to: action.to,
                    subject: action.subject,
                    body: action.body,
                })
            }
            _ => bail!("unsupported unsubscribe action"),
        }
    }

    pub async fn send(&self, account: &Account, draft: &ComposeMessage) -> Result<String> {
        let secret = self.credentials.secret(account).await?;
        let email = build_compose_message(account, draft)?;
        let raw_email = email.formatted();
        let credentials = Credentials::new(account.auth.username().to_owned(), secret.clone());
        let mut transport_builder = match account.smtp_security {
            Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)?,
            Security::StartTls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.smtp_host)?
            }
        }
        .port(account.smtp_port)
        .credentials(credentials);
        if matches!(account.auth, AccountAuth::OAuth2 { .. }) {
            transport_builder = transport_builder.authentication(vec![Mechanism::Xoauth2]);
        }
        let transport = transport_builder.build();
        let response = timeout(
            SMTP_SEND_TIMEOUT,
            transport.send_raw(email.envelope(), &raw_email),
        )
        .await
        .context("SMTP send timed out")??;
        if !smtp_saves_sent_copy(account) {
            self.append_sent_copy(account, &secret, &raw_email)
                .await
                .context(
                    "message was sent, but it could not be saved in the account's Sent folder",
                )?;
        }
        Ok(response.message().collect::<Vec<_>>().join(" "))
    }

    async fn append_sent_copy(
        &self,
        account: &Account,
        secret: &str,
        raw_email: &[u8],
    ) -> Result<()> {
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, secret).await?;
        let listing = client.command("LIST \"\" \"*\"").await.unwrap_or_default();
        let mailbox = sent_mailbox(account, &listing);
        client.append(&mailbox, raw_email).await?;
        let _ = client.command("LOGOUT").await;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MailtoAction {
    to: String,
    subject: String,
    body: String,
}

fn decode_mailto_component(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .context("invalid UTF-8 in unsubscribe email request")
}

fn parse_mailto_action(url: &Url) -> Result<MailtoAction> {
    if url.scheme() != "mailto" || url.host().is_some() || url.password().is_some() {
        bail!("invalid unsubscribe email address");
    }

    let path_to = decode_mailto_component(url.path())?;
    let mut query_to = None;
    let mut subject = None;
    let mut body = None;
    if let Some(query) = url.query() {
        for field in query.split('&') {
            let (name, value) = field.split_once('=').unwrap_or((field, ""));
            let name = decode_mailto_component(name)?;
            let value = decode_mailto_component(value)?;
            if name.eq_ignore_ascii_case("to") && query_to.is_none() {
                query_to = Some(value);
            } else if name.eq_ignore_ascii_case("subject") && subject.is_none() {
                subject = Some(value);
            } else if name.eq_ignore_ascii_case("body") && body.is_none() {
                body = Some(value);
            }
        }
    }

    let to = match (path_to.trim(), query_to.as_deref().map(str::trim)) {
        ("", Some(to)) => to,
        (to, None) => to,
        _ => bail!("ambiguous unsubscribe email recipient"),
    };
    let subject = subject.unwrap_or_default();
    let body = body.unwrap_or_default();
    if to.is_empty()
        || to.len() > 320
        || to.contains(',')
        || to.contains(['\r', '\n', '\0'])
        || subject.len() > 998
        || subject.contains(['\r', '\n', '\0'])
        || body.len() > 64 * 1024
        || body.contains('\0')
        || parse_recipient_mailbox(to).is_err()
    {
        bail!("invalid unsubscribe email address");
    }

    Ok(MailtoAction {
        to: to.to_owned(),
        subject,
        body,
    })
}

fn build_compose_message(account: &Account, draft: &ComposeMessage) -> Result<Message> {
    let from = Mailbox::new(Some(account.display_name.clone()), account.email.parse()?);
    let envelope_from = from.email.clone();
    let mut envelope_to = Vec::new();
    let mut builder = Message::builder().from(from).subject(&draft.subject);
    for recipient in &draft.to {
        let mailbox = parse_recipient_mailbox(recipient)?;
        envelope_to.push(mailbox.email.clone());
        builder = builder.to(mailbox);
    }
    for recipient in &draft.cc {
        let mailbox = parse_recipient_mailbox(recipient)?;
        envelope_to.push(mailbox.email.clone());
        builder = builder.cc(mailbox);
    }
    for recipient in &draft.bcc {
        let mailbox = parse_recipient_mailbox(recipient)?;
        envelope_to.push(mailbox.email.clone());
        builder = builder.bcc(mailbox);
    }
    builder = builder.envelope(Envelope::new(Some(envelope_from), envelope_to)?);
    if let Some(id) = &draft.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(references) = &draft.references {
        builder = builder.references(references.clone());
    }
    let body = if let Some(html) = &draft.body_html {
        MultiPart::alternative()
            .singlepart(SinglePart::plain(draft.body_text.clone()))
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html.clone()),
            )
    } else {
        MultiPart::alternative().singlepart(SinglePart::plain(draft.body_text.clone()))
    };
    let attachments = decode_outbound_attachments(&draft.attachments)?;
    let body =
        attachments
            .into_iter()
            .fold(MultiPart::mixed().multipart(body), |mixed, attachment| {
                mixed.singlepart(
                    LettreAttachment::new(attachment.filename)
                        .body(attachment.bytes, attachment.content_type),
                )
            });
    Ok(builder.multipart(body)?)
}

fn parse_recipient_mailbox(value: &str) -> Result<Mailbox> {
    if let Ok(mailbox) = value.parse() {
        return Ok(mailbox);
    }

    // Several bulk-mail providers (including Amazon SES, Customer.io, Beehiiv,
    // and HubSpot) generate tokenized RFC 2369 unsubscribe recipients whose
    // local part exceeds the historical 64-byte limit enforced by lettre.
    // Re-check the complete dot-atom and domain syntax before using lettre's
    // unchecked constructor; ordinary recipients keep its normal parser.
    let (local, domain) = value
        .split_once('@')
        .context("invalid recipient email address")?;
    let is_dot_atom = !local.is_empty()
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                        | b'.'
                )
        });
    if local.len() <= 64
        || value.len() > 320
        || !is_dot_atom
        || Address::new("validation", domain).is_err()
    {
        bail!("invalid recipient email address");
    }

    Ok(Mailbox::new(
        None,
        Address::new_dangerous(local, domain.to_ascii_lowercase()),
    ))
}

async fn post_one_click_unsubscribe(url: &Url) -> Result<()> {
    if !is_safe_one_click_url(url) {
        bail!("one-click unsubscribe requires a safe HTTPS URL");
    }
    let host = url.host_str().context("unsubscribe URL has no host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        bail!("unsubscribe URL cannot target localhost");
    }
    let port = url
        .port_or_known_default()
        .context("unsubscribe URL has no port")?;
    let addresses = timeout(UNSUBSCRIBE_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .context("unsubscribe DNS lookup timed out")?
        .context("could not resolve unsubscribe host")?;
    let addresses = addresses
        .filter(|address| is_public_address(address.ip()))
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("unsubscribe URL resolves to a private address");
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(UNSUBSCRIBE_TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()?;
    let response = client
        .post(url.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .context("one-click unsubscribe request failed")?;
    if !response.status().is_success() {
        bail!("unsubscribe service returned {}", response.status());
    }
    Ok(())
}

fn is_safe_one_click_url(url: &Url) -> bool {
    url.scheme() == "https" && is_safe_web_url(url)
}

fn is_safe_web_url(url: &Url) -> bool {
    let safe_host = match url.host() {
        Some(Host::Domain(host)) => {
            !host.eq_ignore_ascii_case("localhost") && !host.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => is_public_address(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => is_public_address(IpAddr::V6(address)),
        None => false,
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && safe_host
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4() {
                return is_public_address(IpAddr::V4(address));
            }
            let segments = address.segments();
            let first_segment = address.segments()[0];
            let is_unique_local = first_segment & 0xfe00 == 0xfc00;
            let is_link_local = first_segment & 0xffc0 == 0xfe80;
            let is_site_local = first_segment & 0xffc0 == 0xfec0;
            let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
            let is_benchmark = segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0;
            let is_discard_only =
                segments[0] == 0x0100 && segments[1..4].iter().all(|segment| *segment == 0);
            let is_nat64_well_known = segments[0] == 0x0064
                && segments[1] == 0xff9b
                && segments[2..6].iter().all(|segment| *segment == 0);
            let is_nat64_local = segments[0] == 0x0064
                && segments[1] == 0xff9b
                && segments[2] == 1
                && segments[3..6].iter().all(|segment| *segment == 0);
            let is_transition =
                segments[0] == 0x2002 || (segments[0] == 0x2001 && segments[1] == 0);
            !address.is_loopback()
                && !address.is_unspecified()
                && !is_unique_local
                && !is_link_local
                && !is_site_local
                && !is_documentation
                && !is_benchmark
                && !is_discard_only
                && !is_nat64_well_known
                && !is_nat64_local
                && !is_transition
                && !address.is_multicast()
        }
    }
}

struct DecodedComposeAttachment {
    filename: String,
    bytes: Vec<u8>,
    content_type: ContentType,
}

fn decode_outbound_attachments(
    input: &[ComposeAttachment],
) -> Result<Vec<DecodedComposeAttachment>> {
    if input.len() > MAX_ATTACHMENT_COUNT {
        bail!("a message can include at most {MAX_ATTACHMENT_COUNT} attachments");
    }
    let mut total = 0usize;
    input
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            let bytes = STANDARD
                .decode(&attachment.content_base64)
                .context("attachment data is not valid base64")?;
            if bytes.len() > MAX_OUTBOUND_ATTACHMENT_BYTES {
                bail!(
                    "{} exceeds the {} MiB attachment limit",
                    attachment.filename,
                    MAX_OUTBOUND_ATTACHMENT_BYTES / 1024 / 1024
                );
            }
            total += bytes.len();
            if total > MAX_OUTBOUND_ATTACHMENT_TOTAL_BYTES {
                bail!(
                    "attachments exceed the {} MiB total limit",
                    MAX_OUTBOUND_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
                );
            }
            let mime_type = safe_mime_type(&attachment.mime_type);
            let content_type = ContentType::parse(&mime_type).unwrap_or(ContentType::TEXT_PLAIN);
            Ok(DecodedComposeAttachment {
                filename: safe_attachment_filename(&attachment.filename, index),
                bytes,
                content_type,
            })
        })
        .collect()
}
struct ImapClient {
    reader: BufReader<TlsStream<TcpStream>>,
    tag: u32,
}
struct ImapResponse {
    lines: Vec<String>,
    literal: Option<Vec<u8>>,
}

enum IdleOutcome {
    Changed,
    Renewed,
    Cancelled,
}

struct MailboxPlan {
    remote: String,
    local: &'static str,
    storage: String,
    skip_gmail_system_labels: bool,
}

struct MailboxSyncWork {
    plan: MailboxPlan,
    uid_validity: u32,
    remote_total: usize,
    initialized: bool,
    previous_highest: Option<u32>,
    uids: Vec<u32>,
    offset: usize,
}

impl MailboxPlan {
    fn new(remote: impl Into<String>, local: &'static str) -> Self {
        Self {
            remote: remote.into(),
            local,
            storage: local.into(),
            skip_gmail_system_labels: false,
        }
    }

    fn gmail_archive(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            local: "Archive",
            storage: "Archive".into(),
            skip_gmail_system_labels: true,
        }
    }

    fn discovered(&self, remote: String) -> Self {
        let storage = if remote.eq_ignore_ascii_case(&self.remote) {
            self.local.to_owned()
        } else {
            format!("{}::{remote}", self.local)
        };
        Self {
            remote,
            local: self.local,
            storage,
            skip_gmail_system_labels: self.skip_gmail_system_labels,
        }
    }
}

fn resolve_special_mailboxes(plans: Vec<MailboxPlan>, lines: &[String]) -> Vec<MailboxPlan> {
    let discovered = lines
        .iter()
        .filter_map(|line| parse_list_mailbox(line))
        .collect::<Vec<_>>();
    let mut resolved = Vec::new();
    for plan in plans {
        let flag = match plan.local {
            "Sent" => Some("\\sent"),
            "Drafts" => Some("\\drafts"),
            "Archive" => Some(if plan.skip_gmail_system_labels {
                "\\all"
            } else {
                "\\archive"
            }),
            "Spam" => Some("\\junk"),
            "Trash" => Some("\\trash"),
            _ => None,
        };
        let matches = flag
            .map(|flag| {
                discovered
                    .iter()
                    .filter(|(flags, _)| flags.iter().any(|value| value == flag))
                    .map(|(_, mailbox)| mailbox.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if matches.is_empty() {
            resolved.push(plan);
        } else {
            for remote in matches {
                if !resolved.iter().any(|existing: &MailboxPlan| {
                    existing.local == plan.local && existing.remote.eq_ignore_ascii_case(&remote)
                }) {
                    resolved.push(plan.discovered(remote));
                }
            }
        }
    }
    resolved
}

fn parse_list_mailbox(line: &str) -> Option<(Vec<String>, String)> {
    let list = line.find("LIST (")? + "LIST ".len();
    let attributes_end = line[list..].find(')')? + list;
    let flags = line[list + 1..attributes_end]
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let mut remainder = line[attributes_end + 1..].trim_start();
    let (_, rest) = parse_imap_astring(remainder)?;
    remainder = rest.trim_start();
    let (mailbox, _) = parse_imap_astring(remainder)?;
    Some((flags, mailbox))
}

fn parse_imap_astring(value: &str) -> Option<(String, &str)> {
    if let Some(mut rest) = value.strip_prefix('"') {
        let mut parsed = String::new();
        while !rest.is_empty() {
            let mut chars = rest.chars();
            let character = chars.next()?;
            rest = chars.as_str();
            match character {
                '"' => return Some((parsed, rest)),
                '\\' => {
                    let escaped = rest.chars().next()?;
                    parsed.push(escaped);
                    rest = &rest[escaped.len_utf8()..];
                }
                _ => parsed.push(character),
            }
        }
        None
    } else {
        let end = value.find(char::is_whitespace).unwrap_or(value.len());
        (end > 0).then(|| (value[..end].to_owned(), &value[end..]))
    }
}

fn mailbox_plans(account: &Account) -> Vec<MailboxPlan> {
    if account.provider_id == "gmail" {
        vec![
            MailboxPlan::new("INBOX", "INBOX"),
            MailboxPlan::new("[Gmail]/Sent Mail", "Sent"),
            MailboxPlan::new("[Gmail]/Drafts", "Drafts"),
            MailboxPlan::gmail_archive(&account.archive_mailbox),
            MailboxPlan::new(&account.spam_mailbox, "Spam"),
            MailboxPlan::new("[Gmail]/Trash", "Trash"),
        ]
    } else {
        vec![
            MailboxPlan::new("INBOX", "INBOX"),
            MailboxPlan::new("Sent", "Sent"),
            MailboxPlan::new("Drafts", "Drafts"),
            MailboxPlan::new(&account.archive_mailbox, "Archive"),
            MailboxPlan::new(&account.spam_mailbox, "Spam"),
            MailboxPlan::new("Trash", "Trash"),
        ]
    }
}

fn sent_mailbox(account: &Account, lines: &[String]) -> String {
    let fallback = remote_mailbox(account, "Sent");
    let discovered = lines
        .iter()
        .filter_map(|line| parse_list_mailbox(line))
        .filter(|(flags, _)| flags.iter().any(|flag| flag == "\\sent"))
        .map(|(_, mailbox)| mailbox)
        .collect::<Vec<_>>();
    discovered
        .iter()
        .find(|mailbox| mailbox.eq_ignore_ascii_case(&fallback))
        .cloned()
        .or_else(|| discovered.into_iter().next())
        .unwrap_or(fallback)
}

fn smtp_saves_sent_copy(account: &Account) -> bool {
    // Gmail tells IMAP clients not to append a second copy because submission
    // through smtp.gmail.com automatically adds the message to Gmail/Sent.
    account
        .smtp_host
        .trim_end_matches('.')
        .eq_ignore_ascii_case("smtp.gmail.com")
}

/// Fetch a complete message without changing its `\Seen` flag. IMAP's
/// `RFC822` data item is equivalent to `BODY[]`, which may set that flag;
/// `BODY.PEEK[]` returns the same complete message read-neutrally.
fn hydration_fetch_fields(account: &Account) -> &'static str {
    if account.provider_id == "gmail" {
        "FLAGS INTERNALDATE X-GM-LABELS BODY.PEEK[]"
    } else {
        "FLAGS INTERNALDATE BODY.PEEK[]"
    }
}

fn hydration_fetch_command(account: &Account, uid: u32) -> String {
    format!("UID FETCH {uid} ({})", hydration_fetch_fields(account))
}

fn set_read_command(uid: u32, read: bool) -> String {
    let operation = if read {
        "+FLAGS.SILENT"
    } else {
        "-FLAGS.SILENT"
    };
    format!("UID STORE {uid} {operation} (\\Seen)")
}

fn gmail_all_mail_is_archive(lines: &[String]) -> bool {
    let metadata = lines.join(" ");
    !["\\Inbox", "\\Sent", "\\Draft", "\\Spam", "\\Trash"]
        .iter()
        .any(|label| metadata.contains(label))
}

impl ImapClient {
    async fn connect(account: &Account) -> Result<Self> {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let mut tcp = timeout(
            IMAP_CONNECT_TIMEOUT,
            TcpStream::connect((&*account.imap_host, account.imap_port)),
        )
        .await
        .context("IMAP connection timed out")?
        .context("could not connect to IMAP server")?;
        if account.imap_security == Security::StartTls {
            let mut plain = BufReader::new(tcp);
            let mut greeting = String::new();
            plain.read_line(&mut greeting).await?;
            if !greeting.starts_with("* OK") {
                bail!("IMAP server rejected connection: {}", greeting.trim());
            }
            plain.get_mut().write_all(b"D0000 STARTTLS\r\n").await?;
            plain.get_mut().flush().await?;
            let mut response = String::new();
            plain.read_line(&mut response).await?;
            if !response.starts_with("D0000 OK") {
                bail!("IMAP server rejected STARTTLS: {}", response.trim());
            }
            tcp = plain.into_inner();
        }
        let server_name =
            ServerName::try_from(account.imap_host.clone()).context("invalid IMAP hostname")?;
        let stream = timeout(IMAP_CONNECT_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .context("IMAP TLS handshake timed out")?
            .context("IMAP TLS handshake failed")?;
        let mut reader = BufReader::new(stream);
        if account.imap_security == Security::Tls {
            let mut greeting = String::new();
            reader.read_line(&mut greeting).await?;
            if !greeting.starts_with("* OK") {
                bail!("IMAP server rejected connection: {}", greeting.trim());
            }
        }
        Ok(Self { reader, tag: 0 })
    }

    async fn authenticate(&mut self, account: &Account, secret: &str) -> Result<()> {
        match &account.auth {
            AccountAuth::Password { username } => {
                self.command(&format!(
                    "LOGIN {} {}",
                    quote_imap(username),
                    quote_imap(secret)
                ))
                .await?;
            }
            AccountAuth::OAuth2 { username, .. } => {
                let auth =
                    STANDARD.encode(format!("user={username}\x01auth=Bearer {secret}\x01\x01"));
                self.command(&format!("AUTHENTICATE XOAUTH2 {auth}"))
                    .await?;
            }
        }
        Ok(())
    }

    async fn idle_until_change(
        &mut self,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<IdleOutcome> {
        if *cancel.borrow() {
            return Ok(IdleOutcome::Cancelled);
        }
        self.tag += 1;
        let tag = format!("D{:04}", self.tag);
        self.reader
            .get_mut()
            .write_all(format!("{tag} IDLE\r\n").as_bytes())
            .await?;
        self.reader.get_mut().flush().await?;
        let mut pending_change = false;
        loop {
            let mut continuation = String::new();
            timeout(
                IMAP_COMMAND_TIMEOUT,
                self.reader.read_line(&mut continuation),
            )
            .await
            .context("IMAP IDLE continuation timed out")??;
            if continuation.starts_with('+') {
                break;
            }
            if continuation.to_ascii_uppercase().contains(" EXISTS") {
                pending_change = true;
                continue;
            }
            if continuation.starts_with(&tag)
                || continuation.to_ascii_uppercase().starts_with("* BYE")
            {
                bail!("IMAP server rejected IDLE: {}", continuation.trim());
            }
        }
        let renewal = tokio::time::sleep(IMAP_IDLE_RENEWAL);
        tokio::pin!(renewal);
        let outcome = if pending_change {
            IdleOutcome::Changed
        } else {
            loop {
                let mut line = String::new();
                tokio::select! {
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            break IdleOutcome::Cancelled;
                        }
                    }
                    _ = &mut renewal => break IdleOutcome::Renewed,
                    read = self.reader.read_line(&mut line) => {
                        if read? == 0 {
                            bail!("IMAP connection closed while idling");
                        }
                        if line.to_ascii_uppercase().contains(" EXISTS") {
                            break IdleOutcome::Changed;
                        }
                        if line.to_ascii_uppercase().starts_with("* BYE") {
                            bail!("IMAP server closed the idle connection");
                        }
                    }
                }
            }
        };
        self.reader.get_mut().write_all(b"DONE\r\n").await?;
        self.reader.get_mut().flush().await?;
        loop {
            let mut line = String::new();
            timeout(IMAP_COMMAND_TIMEOUT, self.reader.read_line(&mut line))
                .await
                .context("IMAP IDLE termination timed out")??;
            if line.starts_with(&tag) {
                if !line[tag.len()..].trim_start().starts_with("OK") {
                    bail!("IMAP IDLE termination failed: {}", line.trim());
                }
                break;
            }
        }
        Ok(outcome)
    }

    async fn command(&mut self, command: &str) -> Result<Vec<String>> {
        let response = self.command_with_literal(command).await?;
        Ok(response.lines)
    }

    async fn append(&mut self, mailbox: &str, message: &[u8]) -> Result<()> {
        timeout(IMAP_COMMAND_TIMEOUT, self.append_inner(mailbox, message))
            .await
            .context("IMAP APPEND timed out")?
    }

    async fn append_inner(&mut self, mailbox: &str, message: &[u8]) -> Result<()> {
        self.tag += 1;
        let tag = format!("D{:04}", self.tag);
        self.reader
            .get_mut()
            .write_all(
                format!(
                    "{tag} APPEND {} (\\Seen) {{{}}}\r\n",
                    quote_imap(mailbox),
                    message.len()
                )
                .as_bytes(),
            )
            .await?;
        self.reader.get_mut().flush().await?;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                bail!("IMAP connection closed before APPEND continuation");
            }
            if line.starts_with('+') {
                break;
            }
            if line.starts_with(&tag) {
                bail!("IMAP APPEND failed: {}", line.trim());
            }
            if line.to_ascii_uppercase().starts_with("* BYE") {
                bail!("IMAP server closed the connection before APPEND");
            }
        }
        self.reader.get_mut().write_all(message).await?;
        self.reader.get_mut().write_all(b"\r\n").await?;
        self.reader.get_mut().flush().await?;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                bail!("IMAP connection closed during APPEND");
            }
            if line.starts_with(&tag) {
                if !line[tag.len()..].trim_start().starts_with("OK") {
                    bail!("IMAP APPEND failed: {}", line.trim());
                }
                break;
            }
            if line.to_ascii_uppercase().starts_with("* BYE") {
                bail!("IMAP server closed the connection during APPEND");
            }
        }
        Ok(())
    }

    async fn command_with_literal(&mut self, command: &str) -> Result<ImapResponse> {
        timeout(
            IMAP_COMMAND_TIMEOUT,
            self.command_with_literal_inner(command),
        )
        .await
        .context("IMAP command timed out")?
    }

    async fn command_with_literal_inner(&mut self, command: &str) -> Result<ImapResponse> {
        self.tag += 1;
        let tag = format!("D{:04}", self.tag);
        self.reader
            .get_mut()
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .await?;
        self.reader.get_mut().flush().await?;
        let mut lines = Vec::new();
        let mut literal = None;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                bail!("IMAP connection closed during command");
            }
            if let Some(length) = literal_length(&line) {
                let mut bytes = vec![0; length];
                self.reader.read_exact(&mut bytes).await?;
                literal = Some(bytes);
                let mut tail = String::new();
                self.reader.read_line(&mut tail).await?;
                if !tail.trim().is_empty() {
                    lines.push(tail);
                }
            }
            if line.starts_with(&tag) {
                if !line[tag.len()..].trim_start().starts_with("OK") {
                    bail!("IMAP command failed: {}", line.trim());
                }
                lines.push(line);
                break;
            }
            lines.push(line);
        }
        Ok(ImapResponse { lines, literal })
    }
}

fn literal_length(line: &str) -> Option<usize> {
    let start = line.rfind('{')? + 1;
    let end = line[start..].find('}')? + start;
    line[start..end].parse().ok()
}

fn quote_imap(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn parse_search_uids(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix("* SEARCH "))
        .map(|value| {
            value
                .split_whitespace()
                .filter_map(|uid| uid.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_uid_validity(lines: &[String]) -> Option<u32> {
    lines.iter().find_map(|line| {
        let start = line.find("[UIDVALIDITY ")? + "[UIDVALIDITY ".len();
        let end = line[start..].find(']')? + start;
        line[start..end].trim().parse().ok()
    })
}

fn parse_uid_flags(lines: &[String]) -> Vec<(u32, bool, bool)> {
    lines
        .iter()
        .filter_map(|line| {
            let uid_start = line.find("UID ")? + "UID ".len();
            let uid_end =
                line[uid_start..].find(|character: char| !character.is_ascii_digit())? + uid_start;
            let uid = line[uid_start..uid_end].parse().ok()?;
            let flags_start = line.find("FLAGS (")? + "FLAGS (".len();
            let flags_end = line[flags_start..].find(')')? + flags_start;
            let flags = &line[flags_start..flags_end];
            Some((uid, flags.contains("\\Seen"), flags.contains("\\Flagged")))
        })
        .collect()
}

fn missing_uids_newest_first(remote: &[u32], local: &std::collections::HashSet<u32>) -> Vec<u32> {
    let mut missing = remote
        .iter()
        .filter(|uid| !local.contains(uid))
        .copied()
        .collect::<Vec<_>>();
    missing.sort_unstable_by(|left, right| right.cmp(left));
    missing
}

fn supports_idle(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        line.to_ascii_uppercase()
            .split_whitespace()
            .any(|item| item == "IDLE")
    })
}

fn parse_internal_date(lines: &[String]) -> Option<chrono::DateTime<Utc>> {
    lines.iter().find_map(|line| {
        let value = line.split_once("INTERNALDATE \"")?.1.split_once('"')?.0;
        chrono::DateTime::parse_from_str(value, "%d-%b-%Y %H:%M:%S %z")
            .ok()
            .map(|date| date.with_timezone(&Utc))
    })
}

fn message_received_at(
    parsed: &mailparse::ParsedMail<'_>,
    response_lines: &[String],
) -> Result<chrono::DateTime<Utc>> {
    if let Some(date) = parsed
        .headers
        .get_first_value("Date")
        .and_then(|date| mail_parser::DateTime::parse_rfc822(&date))
        .and_then(|date| chrono::DateTime::from_timestamp(date.to_timestamp(), 0))
    {
        return Ok(date);
    }
    parse_internal_date(response_lines).context(
        "message has no valid Date header and the IMAP server omitted a valid INTERNALDATE",
    )
}

fn sync_uids(mut uids: Vec<u32>, highest_uid: Option<u32>, max_messages: u32) -> Vec<u32> {
    uids.sort_unstable();
    let limit = max_messages as usize;
    if let Some(highest_uid) = highest_uid {
        // Drain incremental batches from the oldest unseen UID so advancing
        // the durable watermark cannot skip messages beyond this batch.
        uids.retain(|uid| *uid > highest_uid);
        uids.truncate(limit);
        uids
    } else {
        // Initial sync remains silent and starts with the newest messages.
        let offset = uids.len().saturating_sub(limit);
        uids.split_off(offset)
    }
}

fn parse_copy_uid(lines: &[String]) -> Option<u32> {
    lines.iter().find_map(|line| {
        let start = line.find("[COPYUID ")? + "[COPYUID ".len();
        let end = line[start..].find(']')? + start;
        line[start..end].split_whitespace().nth(2)?.parse().ok()
    })
}

async fn parse_message(
    account: &Account,
    mailbox: &str,
    uid: u32,
    response_lines: &[String],
    raw: &[u8],
    dkim_authenticator: Option<&MessageAuthenticator>,
) -> Result<MailSummary> {
    let parsed = mailparse::parse_mail(raw)?;
    let header = |name| parsed.headers.get_first_value(name).unwrap_or_default();
    let received_at = message_received_at(&parsed, response_lines)?;
    let from_header = header("From");
    let (from_name, from_address) = parse_first_address(&from_header);
    let id = stable_message_id(account.id, mailbox, uid);
    let body_html = extract_html(&parsed)?
        .map(|html| resolve_inline_images(html, &parsed))
        .transpose()?;
    let mut body_text = extract_text(&parsed)?;
    if body_text.trim().is_empty() {
        if let Some(html) = &body_html {
            body_text = mail_parser::decoders::html::html_to_text(html);
        }
    }
    let flags = response_lines.join(" ");
    let (unsubscribe_kind, unsubscribe_url) =
        extract_unsubscribe(&parsed, raw, dkim_authenticator).await;
    let message_id = canonical_message_ids(&header("Message-ID"));
    let in_reply_to = canonical_message_ids(&header("In-Reply-To"));
    let reference_ids = canonical_message_ids(&header("References"));
    Ok(MailSummary {
        id: id.clone(),
        account_id: account.id.to_string(),
        mailbox: mailbox.to_owned(),
        uid: uid as i64,
        message_id,
        in_reply_to,
        reference_ids,
        thread_id: id.clone(),
        subject: header("Subject"),
        from_name,
        from_address,
        to_addresses: header("To"),
        cc_addresses: header("Cc"),
        bcc_addresses: header("Bcc"),
        reply_to_addresses: header("Reply-To"),
        received_at,
        snippet: clean_snippet(&body_text),
        body_text,
        body_html,
        content_state: "complete".into(),
        unsubscribe_kind,
        unsubscribe_url,
        is_read: flags.contains("\\Seen"),
        is_flagged: flags.contains("\\Flagged"),
        has_attachments: has_attachment(&parsed),
        attachments: extract_attachments(&parsed, &id)?,
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: classification_signals(&parsed, response_lines),
    })
}

fn parse_catalog_message(
    account: &Account,
    mailbox: &str,
    uid: u32,
    response_lines: &[String],
    raw_headers: &[u8],
    snippet: String,
) -> Result<MailSummary> {
    let parsed = mailparse::parse_mail(raw_headers)?;
    let header = |name| parsed.headers.get_first_value(name).unwrap_or_default();
    let received_at = message_received_at(&parsed, response_lines)?;
    let (from_name, from_address) = parse_first_address(&header("From"));
    let id = stable_message_id(account.id, mailbox, uid);
    let flags = response_lines.join(" ");
    let structure = flags.to_ascii_lowercase();
    let has_attachments = structure.contains("attachment")
        || structure.contains(" filename ")
        || structure.contains(" name ");
    let unsubscribe_url = parsed
        .headers
        .get_all_values("List-Unsubscribe")
        .into_iter()
        .flat_map(|value| parse_list_urls(&value))
        .find(|url| matches!(url.scheme(), "https" | "http" | "mailto"));
    let unsubscribe_kind = unsubscribe_url.as_ref().map(|url| match url.scheme() {
        "mailto" => "mailto".to_owned(),
        _ => "web".to_owned(),
    });
    let message_id = canonical_message_ids(&header("Message-ID"));
    let in_reply_to = canonical_message_ids(&header("In-Reply-To"));
    let reference_ids = canonical_message_ids(&header("References"));
    let provisional_thread_id = reference_ids
        .as_deref()
        .and_then(|references| references.split_whitespace().next())
        .or(in_reply_to.as_deref())
        .or(message_id.as_deref())
        .unwrap_or(&id)
        .to_owned();
    Ok(MailSummary {
        id: id.clone(),
        account_id: account.id.to_string(),
        mailbox: mailbox.to_owned(),
        uid: uid as i64,
        message_id,
        in_reply_to,
        reference_ids,
        thread_id: provisional_thread_id,
        subject: header("Subject"),
        from_name,
        from_address,
        to_addresses: header("To"),
        cc_addresses: header("Cc"),
        bcc_addresses: header("Bcc"),
        reply_to_addresses: header("Reply-To"),
        received_at,
        snippet,
        body_text: String::new(),
        body_html: None,
        content_state: "complete".into(),
        unsubscribe_kind,
        unsubscribe_url: unsubscribe_url.map(|url| url.to_string()),
        is_read: flags.contains("\\Seen"),
        is_flagged: flags.contains("\\Flagged"),
        has_attachments,
        attachments: Vec::new(),
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: classification_signals(&parsed, response_lines),
    })
}

fn snippet_from_partial(raw: &[u8]) -> String {
    parsed_snippet(raw)
        .filter(|snippet| !looks_like_mime_artifact(snippet))
        .or_else(|| {
            partial_text_parts(raw)
                .into_iter()
                .filter_map(|part| parsed_snippet(part.as_bytes()))
                .find(|snippet| !looks_like_mime_artifact(snippet))
        })
        .map(|text| clean_snippet(&text))
        .unwrap_or_default()
}

fn parsed_snippet(raw: &[u8]) -> Option<String> {
    let parsed = mailparse::parse_mail(raw).ok()?;
    let plain = extract_text(&parsed).ok()?;
    if !plain.trim().is_empty() {
        return Some(plain);
    }
    extract_html(&parsed)
        .ok()
        .flatten()
        .map(|html| mail_parser::decoders::html::html_to_text(&html))
}

/// A partial RFC822 fetch often ends before a multipart message's closing
/// boundary. `mailparse` correctly rejects that incomplete container, but the
/// first text leaf can still be complete enough to decode. Extract only MIME
/// leaf blocks whose headers explicitly identify text; never expose the raw
/// partial payload as a user-facing preview.
fn partial_text_parts(raw: &[u8]) -> Vec<String> {
    let normalized = String::from_utf8_lossy(raw).replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut parts = Vec::new();
    for start in 0..lines.len() {
        if start > 0 && !lines[start - 1].trim_start().starts_with("--") {
            continue;
        }
        let Some(header_end) = (start..lines.len()).find(|index| lines[*index].is_empty()) else {
            continue;
        };
        let headers = lines[start..header_end].join("\n");
        let lowercase = headers.to_ascii_lowercase();
        if !lowercase.contains("content-type: text/plain")
            && !lowercase.contains("content-type: text/html")
        {
            continue;
        }
        let body_end = ((header_end + 1)..lines.len())
            .find(|index| lines[*index].trim_start().starts_with("--"))
            .unwrap_or(lines.len());
        parts.push(lines[start..body_end].join("\r\n"));
    }
    parts
}

fn looks_like_mime_artifact(text: &str) -> bool {
    let lowercase = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    text.trim_start().starts_with("--")
        || lowercase.contains("content-type:")
        || lowercase.contains("content-transfer-encoding:")
        || lowercase.contains("this is a multipart message in mime format")
        || bytes.windows(3).any(|window| {
            window[0] == b'=' && window[1].is_ascii_hexdigit() && window[2].is_ascii_hexdigit()
        })
}

fn clean_snippet(text: &str) -> String {
    let text = if looks_like_html(text) {
        mail_parser::decoders::html::html_to_text(text)
    } else {
        text.to_owned()
    };
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn looks_like_html(text: &str) -> bool {
    text.as_bytes().windows(2).any(|pair| {
        pair[0] == b'<' && (pair[1].is_ascii_alphabetic() || matches!(pair[1], b'/' | b'!'))
    })
}

fn parse_header_message(
    account: &Account,
    mailbox: &str,
    uid: u32,
    response_lines: &[String],
    raw_headers: &[u8],
) -> Result<MailSummary> {
    let parsed = mailparse::parse_mail(raw_headers)?;
    let header = |name| parsed.headers.get_first_value(name).unwrap_or_default();
    let from_header = header("From");
    let (from_name, from_address) = parse_first_address(&from_header);
    let id = stable_message_id(account.id, mailbox, uid);
    let flags = response_lines.join(" ");
    Ok(MailSummary {
        id: id.clone(),
        account_id: account.id.to_string(),
        mailbox: mailbox.to_owned(),
        uid: uid as i64,
        message_id: canonical_message_ids(&header("Message-ID")),
        in_reply_to: canonical_message_ids(&header("In-Reply-To")),
        reference_ids: canonical_message_ids(&header("References")),
        thread_id: id,
        subject: header("Subject"),
        from_name,
        from_address,
        to_addresses: header("To"),
        cc_addresses: header("Cc"),
        bcc_addresses: header("Bcc"),
        reply_to_addresses: header("Reply-To"),
        received_at: message_received_at(&parsed, response_lines)?,
        snippet: String::new(),
        body_text: String::new(),
        body_html: None,
        content_state: "headers_only".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: flags.contains("\\Seen"),
        is_flagged: flags.contains("\\Flagged"),
        has_attachments: false,
        attachments: Vec::new(),
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: classification_signals(&parsed, response_lines),
    })
}

fn canonical_message_ids(value: &str) -> Option<String> {
    let ids = mailparse::msgidparse(value).ok()?;
    (!ids.is_empty()).then(|| ids.to_string())
}

#[cfg(test)]
fn parse_threading_headers(raw: &[u8]) -> Result<ThreadingHeaders> {
    let parsed = mailparse::parse_mail(raw)?;
    let header = |name| parsed.headers.get_first_value(name).unwrap_or_default();
    Ok(ThreadingHeaders {
        message_id: canonical_message_ids(&header("Message-ID")),
        in_reply_to: canonical_message_ids(&header("In-Reply-To")),
        reference_ids: canonical_message_ids(&header("References")),
    })
}

async fn extract_unsubscribe(
    parsed: &mailparse::ParsedMail<'_>,
    raw: &[u8],
    dkim_authenticator: Option<&MessageAuthenticator>,
) -> (Option<String>, Option<String>) {
    let unsubscribe_headers = parsed
        .headers
        .get_all_values("List-Unsubscribe")
        .into_iter()
        .collect::<Vec<_>>();
    let urls = unsubscribe_headers
        .iter()
        .flat_map(|value| parse_list_urls(value))
        .collect::<Vec<_>>();
    if urls.is_empty() {
        return (None, None);
    }

    let post_headers = parsed.headers.get_all_values("List-Unsubscribe-Post");
    // RFC 8058 requires one instance of each header. With duplicates, a DKIM
    // signature can cover a different occurrence from the URL we selected.
    let one_click_requested = unsubscribe_headers.len() == 1
        && post_headers.len() == 1
        && post_headers[0].trim() == "List-Unsubscribe=One-Click";
    if one_click_requested {
        if let Some(url) = urls.iter().find(|url| is_safe_one_click_url(url)) {
            if dkim_covers_unsubscribe_headers(raw, dkim_authenticator).await {
                return (Some("one_click".into()), Some(url.to_string()));
            }
        }
    }

    for url in urls {
        match url.scheme() {
            "https" | "http" if is_safe_web_url(&url) => {
                return (Some("web".into()), Some(url.to_string()));
            }
            "mailto" if parse_mailto_action(&url).is_ok() => {
                return (Some("mailto".into()), Some(url.to_string()));
            }
            _ => {}
        }
    }
    (None, None)
}

fn parse_list_urls(value: &str) -> Vec<Url> {
    let mut urls = Vec::new();
    let mut value = value;
    while let Some((_, rest)) = value.split_once('<') {
        let Some((url, tail)) = rest.split_once('>') else {
            break;
        };
        if let Ok(url) = Url::parse(url.trim()) {
            urls.push(url);
        }
        value = tail;
    }
    urls
}

async fn dkim_covers_unsubscribe_headers(
    raw: &[u8],
    dkim_authenticator: Option<&MessageAuthenticator>,
) -> bool {
    let (Some(authenticator), Some(message)) =
        (dkim_authenticator, AuthenticatedMessage::parse(raw))
    else {
        return false;
    };
    let Ok(results) = timeout(DKIM_VERIFY_TIMEOUT, authenticator.verify_dkim(&message)).await
    else {
        return false;
    };
    results.iter().any(|result| {
        matches!(result.result(), DkimResult::Pass)
            && result.signature().is_some_and(|signature| {
                signature
                    .h
                    .iter()
                    .any(|header| header.eq_ignore_ascii_case("List-Unsubscribe"))
                    && signature
                        .h
                        .iter()
                        .any(|header| header.eq_ignore_ascii_case("List-Unsubscribe-Post"))
            })
    })
}

/// Extract only category-relevant structure from RFC headers. Values such as
/// unsubscribe URLs are intentionally not retained: their presence is the
/// useful signal, and the model does not need a per-recipient tracking URL.
fn classification_signals(mail: &mailparse::ParsedMail<'_>, response_lines: &[String]) -> String {
    let headers = &mail.headers;
    let mut signals = Vec::new();
    if headers.get_first_value("List-Unsubscribe").is_some() {
        signals.push("Mailing-list unsubscribe header present");
    }
    if headers.get_first_value("List-Id").is_some() {
        signals.push("Mailing-list identifier header present");
    }
    if let Some(value) = headers.get_first_value("Precedence") {
        signals.push(match value.trim().to_ascii_lowercase().as_str() {
            "bulk" => "Bulk-mail precedence header",
            "list" => "Mailing-list precedence header",
            _ => "Precedence header present",
        });
    }
    if let Some(value) = headers.get_first_value("Auto-Submitted") {
        signals.push(match value.trim().to_ascii_lowercase().as_str() {
            "auto-generated" => "Automatically generated message header",
            "auto-replied" => "Automatically replied message header",
            _ => "Auto-Submitted header present",
        });
    }
    if headers.get_first_value("In-Reply-To").is_some() {
        signals.push("Reply thread header present");
    }
    if headers.get_first_value("Reply-To").is_some() {
        signals.push("Reply-To header present");
    }
    // Gmail exposes server-side category labels through X-GM-LABELS. Retain
    // only their broad category, never user labels or message identifiers.
    if let Some(category) = gmail_category_signal(response_lines) {
        signals.push(category);
    }
    signals.join("\n")
}

fn gmail_category_signal(response_lines: &[String]) -> Option<&'static str> {
    let metadata = response_lines.join(" ").to_ascii_lowercase();
    if metadata.contains("\\category_personal") {
        Some("Gmail category: Personal")
    } else if metadata.contains("\\category_promotions") {
        Some("Gmail category: Promotions")
    } else if metadata.contains("\\category_social") {
        Some("Gmail category: Social")
    } else if metadata.contains("\\category_updates") {
        Some("Gmail category: Updates")
    } else if metadata.contains("\\category_forums") {
        Some("Gmail category: Forums")
    } else {
        None
    }
}

fn merge_gmail_category_signal(existing: &str, gmail_category: Option<&str>) -> String {
    let mut signals: Vec<&str> = existing
        .lines()
        .filter(|signal| !signal.starts_with(GMAIL_CATEGORY_SIGNAL_PREFIX))
        .collect();
    if let Some(category) = gmail_category {
        signals.push(category);
    }
    signals.join("\n")
}

fn parse_first_address(value: &str) -> (Option<String>, String) {
    match mailparse::addrparse(value)
        .ok()
        .and_then(|list| list.first().cloned())
    {
        Some(mailparse::MailAddr::Single(info)) => (info.display_name, info.addr),
        Some(mailparse::MailAddr::Group(group)) => group
            .addrs
            .first()
            .map(|info| (info.display_name.clone(), info.addr.clone()))
            .unwrap_or((None, value.to_owned())),
        None => (None, value.to_owned()),
    }
}

fn extract_text(mail: &mailparse::ParsedMail<'_>) -> Result<String> {
    if mail.subparts.is_empty() {
        return if mail.ctype.mimetype.eq_ignore_ascii_case("text/plain")
            && !is_attachment_part(mail)
        {
            Ok(mail.get_body()?)
        } else {
            Ok(String::new())
        };
    }
    let parts = mail
        .subparts
        .iter()
        .map(extract_text)
        .collect::<Result<Vec<_>>>()?;
    Ok(parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn extract_html(mail: &mailparse::ParsedMail<'_>) -> Result<Option<String>> {
    if mail.subparts.is_empty() {
        return if mail.ctype.mimetype.eq_ignore_ascii_case("text/html") && !is_attachment_part(mail)
        {
            Ok(Some(mail.get_body()?))
        } else {
            Ok(None)
        };
    }
    for part in &mail.subparts {
        if let Some(html) = extract_html(part)? {
            return Ok(Some(html));
        }
    }
    Ok(None)
}

fn resolve_inline_images(mut html: String, mail: &mailparse::ParsedMail<'_>) -> Result<String> {
    let mut images = Vec::new();
    collect_inline_images(mail, &mut images)?;
    for (reference, data_url) in images {
        html = replace_ascii_case_insensitive(&html, &reference, &data_url);
    }
    Ok(html)
}

fn collect_inline_images(
    mail: &mailparse::ParsedMail<'_>,
    images: &mut Vec<(String, String)>,
) -> Result<()> {
    if mail.subparts.is_empty()
        && mail
            .ctype
            .mimetype
            .to_ascii_lowercase()
            .starts_with("image/")
    {
        let references = [
            mail.headers
                .get_first_value("Content-ID")
                .map(|value| format!("cid:{}", value.trim().trim_matches(['<', '>']))),
            mail.headers.get_first_value("Content-Location"),
        ];
        let encoded = STANDARD.encode(mail.get_body_raw()?);
        let data_url = format!("data:{};base64,{encoded}", mail.ctype.mimetype);
        for reference in references.into_iter().flatten() {
            if !reference.is_empty() {
                images.push((reference, data_url.clone()));
            }
        }
    }
    for part in &mail.subparts {
        collect_inline_images(part, images)?;
    }
    Ok(())
}

fn replace_ascii_case_insensitive(input: &str, needle: &str, replacement: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let lower_input = input.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(offset) = lower_input[cursor..].find(&lower_needle) {
        let start = cursor + offset;
        output.push_str(&input[cursor..start]);
        output.push_str(replacement);
        cursor = start + needle.len();
    }
    output.push_str(&input[cursor..]);
    output
}

fn has_attachment(mail: &mailparse::ParsedMail<'_>) -> bool {
    is_attachment_part(mail) || mail.subparts.iter().any(has_attachment)
}

fn is_attachment_part(mail: &mailparse::ParsedMail<'_>) -> bool {
    let disposition = mail.get_content_disposition();
    let is_unnamed_text_body = matches!(
        mail.ctype.mimetype.to_ascii_lowercase().as_str(),
        "text/plain" | "text/html"
    ) && !disposition.params.contains_key("filename")
        && !mail.ctype.params.contains_key("name")
        && mail.headers.get_first_value("Content-ID").is_none();
    disposition.disposition == mailparse::DispositionType::Attachment
        || disposition.params.contains_key("filename")
        || mail.ctype.params.contains_key("name")
        || mail.headers.get_first_value("Content-ID").is_some()
        || (mail
            .headers
            .get_first_value("Content-Disposition")
            .is_some()
            && disposition.disposition == mailparse::DispositionType::Inline
            && !is_unnamed_text_body)
}

fn extract_attachments(
    mail: &mailparse::ParsedMail<'_>,
    message_id: &str,
) -> Result<Vec<AttachmentData>> {
    let mut parts = Vec::new();
    collect_attachment_parts(mail, message_id, &mut parts)?;
    Ok(parts)
}

fn collect_attachment_parts(
    mail: &mailparse::ParsedMail<'_>,
    message_id: &str,
    parts: &mut Vec<AttachmentData>,
) -> Result<()> {
    if mail.subparts.is_empty() {
        if !is_attachment_part(mail) {
            return Ok(());
        }
        if parts.len() >= MAX_ATTACHMENT_COUNT {
            bail!("message has more than {MAX_ATTACHMENT_COUNT} attachments");
        }
        let bytes = mail.get_body_raw()?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment exceeds the {} MiB safety limit",
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            );
        }
        let disposition = mail.get_content_disposition();
        let supplied_name = disposition
            .params
            .get("filename")
            .or_else(|| mail.ctype.params.get("name"))
            .map(String::as_str)
            .unwrap_or("attachment");
        let filename = safe_attachment_filename(supplied_name, parts.len());
        let mime_type = safe_mime_type(&mail.ctype.mimetype);
        let is_inline = matches!(disposition.disposition, mailparse::DispositionType::Inline)
            || mail.headers.get_first_value("Content-ID").is_some();
        let attachment = Attachment {
            id: format!("{message_id}:{}", parts.len()),
            message_id: message_id.to_owned(),
            is_potentially_unsafe: is_potentially_unsafe(&filename, &mime_type),
            filename,
            mime_type,
            size_bytes: bytes.len() as i64,
            is_inline,
        };
        parts.push(AttachmentData { attachment, bytes });
        return Ok(());
    }
    for part in &mail.subparts {
        collect_attachment_parts(part, message_id, parts)?;
    }
    Ok(())
}

pub fn safe_attachment_filename(value: &str, index: usize) -> String {
    let value = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        .collect::<String>();
    let value = value.trim().trim_matches('.').trim();
    let value = if value.is_empty() {
        "attachment"
    } else {
        value
    };
    let truncated = value.chars().take(180).collect::<String>();
    if truncated.is_empty() {
        format!("attachment-{}", index + 1)
    } else {
        truncated
    }
}

pub fn safe_mime_type(value: &str) -> String {
    let value = value.trim();
    let valid = value.split_once('/').is_some_and(|(kind, subtype)| {
        !kind.is_empty()
            && !subtype.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '/' | '.' | '+' | '-' | '_')
            })
    });
    if valid {
        value.to_ascii_lowercase()
    } else {
        "application/octet-stream".into()
    }
}

pub fn is_potentially_unsafe(filename: &str, mime_type: &str) -> bool {
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "app"
            | "bat"
            | "cmd"
            | "com"
            | "command"
            | "exe"
            | "js"
            | "jse"
            | "msi"
            | "ps1"
            | "scpt"
            | "sh"
            | "vbs"
            | "wsf"
    ) || matches!(
        mime_type,
        "application/x-msdownload" | "application/x-sh" | "application/x-apple-diskimage"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider, AccountDraft};

    fn test_account() -> Account {
        AccountDraft {
            email: "reader@example.test".into(),
            display_name: "Reader".into(),
            provider_id: Some("fastmail".into()),
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
        .into_account(provider::by_id("fastmail").unwrap())
    }

    #[tokio::test]
    async fn configures_the_dkim_resolver_with_the_app_crypto_provider() {
        let service = MailService::new(Store::in_memory().await.unwrap());
        assert!(service.dkim_authenticator.is_some());
    }

    #[tokio::test]
    async fn retains_decoded_recipient_headers_in_complete_catalogue_and_header_messages() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: Sender <sender@example.test>\r\n",
            "To: Primary <primary@example.test>,\r\n",
            " Team: second@example.test, Third <third@example.test>;\r\n",
            "Cc: =?UTF-8?B?TcOkcmE=?= <mara@example.test>,\r\n",
            " Other <other@example.test>\r\n",
            "Bcc: Hidden <hidden@example.test>\r\n",
            "Reply-To: Replies <replies@example.test>\r\n",
            "Subject: Recipients\r\n\r\n",
            "Hello"
        );

        let complete = parse_message(&account, "INBOX", 1, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        let catalogue =
            parse_catalog_message(&account, "INBOX", 2, &[], raw.as_bytes(), String::new())
                .unwrap();
        let headers = parse_header_message(&account, "INBOX", 3, &[], raw.as_bytes()).unwrap();

        for message in [complete, catalogue, headers] {
            assert_eq!(message.to_addresses, "Primary <primary@example.test>, Team: second@example.test, Third <third@example.test>;");
            assert_eq!(
                message.cc_addresses,
                "Mära <mara@example.test>, Other <other@example.test>"
            );
            assert_eq!(message.bcc_addresses, "Hidden <hidden@example.test>");
            assert_eq!(message.reply_to_addresses, "Replies <replies@example.test>");
        }
    }

    #[test]
    fn missing_recipient_headers_remain_empty_without_fallbacks() {
        let account = test_account();
        let message = parse_header_message(
            &account,
            "INBOX",
            1,
            &[],
            b"From: Sender <sender@example.test>\r\nDate: Tue, 21 Jul 2026 10:00:00 +0000\r\n\r\n",
        )
        .unwrap();

        assert!(message.to_addresses.is_empty());
        assert!(message.cc_addresses.is_empty());
        assert!(message.bcc_addresses.is_empty());
        assert!(message.reply_to_addresses.is_empty());
    }

    #[test]
    fn shares_provider_mailbox_mapping_with_clients() {
        let gmail = AccountDraft {
            email: "person@gmail.com".into(),
            display_name: "Person".into(),
            provider_id: Some("gmail".into()),
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
        .into_account(provider::by_id("gmail").unwrap());
        assert_eq!(remote_mailbox(&gmail, "Archive"), "[Gmail]/All Mail");
        assert_eq!(remote_mailbox(&gmail, "Trash"), "[Gmail]/Trash");
        assert_eq!(
            mailbox_action_destination(MailboxAction::Archive),
            Some("Archive")
        );
        assert_eq!(
            mailbox_action_destination(MailboxAction::NotSpam),
            Some("INBOX")
        );
        assert_eq!(mailbox_action_destination(MailboxAction::Delete), None);
    }

    #[test]
    fn resolves_the_sent_folder_discovered_for_the_sending_account() {
        let custom = AccountDraft {
            email: "person@example.com".into(),
            display_name: "Person".into(),
            provider_id: Some("custom".into()),
            username: None,
            imap_host: Some("imap.example.com".into()),
            imap_port: Some(993),
            imap_security: Some(Security::Tls),
            smtp_host: Some("smtp.example.com".into()),
            smtp_port: Some(465),
            smtp_security: Some(Security::Tls),
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("custom").unwrap());
        let listing = vec![
            r#"* LIST (\HasNoChildren) "/" "INBOX""#.into(),
            r#"* LIST (\HasNoChildren \Sent) "/" "Sent Messages""#.into(),
        ];

        assert_eq!(sent_mailbox(&custom, &listing), "Sent Messages");
        assert_eq!(sent_mailbox(&custom, &[]), "Sent");
    }

    #[test]
    fn automatic_hydration_fetches_are_read_neutral_for_gmail_and_generic_imap() {
        let gmail = AccountDraft {
            email: "person@gmail.com".into(),
            display_name: "Person".into(),
            provider_id: Some("gmail".into()),
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
        .into_account(provider::by_id("gmail").unwrap());
        let generic = AccountDraft {
            email: "person@example.com".into(),
            display_name: "Person".into(),
            provider_id: Some("custom".into()),
            username: None,
            imap_host: Some("imap.example.com".into()),
            imap_port: Some(993),
            imap_security: Some(Security::Tls),
            smtp_host: Some("smtp.example.com".into()),
            smtp_port: Some(465),
            smtp_security: Some(Security::Tls),
            archive_mailbox: None,
            spam_mailbox: None,
        }
        .into_account(provider::by_id("custom").unwrap());

        let gmail_command = hydration_fetch_command(&gmail, 42);
        let generic_command = hydration_fetch_command(&generic, 42);
        assert_eq!(
            gmail_command,
            "UID FETCH 42 (FLAGS INTERNALDATE X-GM-LABELS BODY.PEEK[])"
        );
        assert_eq!(
            generic_command,
            "UID FETCH 42 (FLAGS INTERNALDATE BODY.PEEK[])"
        );
        for command in [gmail_command, generic_command] {
            assert!(command.contains("BODY.PEEK[]"));
            assert!(!command.contains("RFC822"));
        }
    }

    #[test]
    fn explicit_read_changes_still_update_the_seen_flag() {
        assert_eq!(
            set_read_command(42, true),
            "UID STORE 42 +FLAGS.SILENT (\\Seen)"
        );
        assert_eq!(
            set_read_command(42, false),
            "UID STORE 42 -FLAGS.SILENT (\\Seen)"
        );
    }

    #[test]
    fn avoids_duplicate_append_when_the_smtp_provider_saves_sent_mail() {
        let mut gmail = AccountDraft {
            email: "person@gmail.com".into(),
            display_name: "Person".into(),
            provider_id: Some("gmail".into()),
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
        .into_account(provider::by_id("gmail").unwrap());

        assert!(smtp_saves_sent_copy(&gmail));
        gmail.smtp_host = "smtp.example.com".into();
        assert!(!smtp_saves_sent_copy(&gmail));
    }

    #[test]
    fn extracts_literal_lengths() {
        assert_eq!(literal_length("* 1 FETCH (RFC822 {42}\r\n"), Some(42));
    }
    #[test]
    fn parses_search() {
        assert_eq!(
            parse_search_uids(&["* SEARCH 4 8 15\r\n".into()]),
            vec![4, 8, 15]
        );
    }

    #[test]
    fn realtime_snapshot_fetches_only_uids_above_the_watermark() {
        // Realtime reconciliation searches the complete mailbox so it can
        // remove archived UIDs. The fetch batch must still contain only new
        // mail, even when SEARCH ALL returns older UIDs out of order.
        assert_eq!(sync_uids(vec![12, 7, 11, 10], Some(10), 25), vec![11, 12]);
        assert_eq!(sync_uids(vec![12, 7, 11, 10], Some(10), 1), vec![11]);
    }

    #[test]
    fn parses_uidvalidity_and_flag_refreshes() {
        assert_eq!(
            parse_uid_validity(&["* OK [UIDVALIDITY 98765] UIDs valid\r\n".into()]),
            Some(98765)
        );
        assert_eq!(
            parse_uid_flags(&[
                "* 1 FETCH (UID 41 FLAGS (\\Seen))\r\n".into(),
                "* 2 FETCH (UID 42 FLAGS (\\Flagged))\r\n".into(),
            ]),
            vec![(41, true, false), (42, false, true)]
        );
    }

    #[test]
    fn full_catalogue_backfill_is_newest_first_and_not_capped() {
        let local = [2].into_iter().collect();
        assert_eq!(
            missing_uids_newest_first(&[1, 2, 3, 4, 5], &local),
            vec![5, 4, 3, 1]
        );
    }

    #[test]
    fn partial_snippets_are_normalized_and_bounded() {
        let raw = format!(
            "Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hello   Tallinn</p>{}",
            " x".repeat(400)
        );
        let snippet = snippet_from_partial(raw.as_bytes());
        assert!(snippet.starts_with("Hello Tallinn"));
        assert_eq!(snippet.chars().count(), 240);
    }

    #[test]
    fn partial_snippets_decode_mime_transfer_encoding() {
        let raw = b"From: sender@example.com\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
PHA+WW91IGhhdmUgYSBtZXNzYWdlIGZyb20gQXBhcnRhbWVudHkgR8OzcnNraSBQcmVzdGlnZTwvcD4=";

        assert_eq!(
            snippet_from_partial(raw),
            "You have a message from Apartamenty Górski Prestige"
        );
    }

    #[test]
    fn partial_snippets_decode_quoted_printable_text() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
We need you to confirm some details=0A=0APlease review your profile.";

        assert_eq!(
            snippet_from_partial(raw),
            "We need you to confirm some details Please review your profile."
        );
    }

    #[test]
    fn partial_snippets_decode_utf8_quoted_printable() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Tere, pakk ootab v=C3=A4ljav=C3=B5tmist!";

        assert_eq!(snippet_from_partial(raw), "Tere, pakk ootab väljavõtmist!");
    }

    #[test]
    fn truncated_multipart_uses_the_first_decodable_text_leaf() {
        let raw = b"From: sender@example.com\r\n\
Content-Type: multipart/alternative; boundary=\"swift-boundary\"\r\n\
\r\n\
--swift-boundary\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
MEELDETULETUS: UNISEND pakk ootab v=C3=A4ljav=C3=B5tmist!";

        assert_eq!(
            snippet_from_partial(raw),
            "MEELDETULETUS: UNISEND pakk ootab väljavõtmist!"
        );
    }

    #[test]
    fn truncated_nested_multipart_does_not_leak_boundaries_or_headers() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Your order is ready=2E";

        assert_eq!(snippet_from_partial(raw), "Your order is ready.");
    }

    #[test]
    fn truncated_multipart_can_fall_back_to_an_html_leaf() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"body\"\r\n\
\r\n\
--body\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<p>Hello <strong>Alex</strong>, your shipment is ready.</p>";

        assert_eq!(
            snippet_from_partial(raw),
            "Hello Alex, your shipment is ready."
        );
    }

    #[test]
    fn malformed_mime_returns_an_empty_snippet_instead_of_raw_bytes() {
        let raw = b"--broken-boundary\r\n\
Content-Type: multipart/alternative; boundary=\"missing\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
=0A=0A";

        assert_eq!(snippet_from_partial(raw), "");
    }

    #[test]
    fn incomplete_encoded_body_returns_empty_instead_of_encoded_content() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
VGhpcyBpcyBhbiBpbmNvbXBsZXRlIGJhc2U2NCBz";

        let snippet = snippet_from_partial(raw);
        assert!(!snippet.contains("Content-Type"));
        assert!(!snippet.starts_with("VGhp"));
    }

    #[test]
    fn representative_mime_shapes_are_rendered_as_text_not_transport_syntax() {
        let samples = [
            (
                b"--6a623969_2eb141f2_c01e\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
Content-Disposition: inline\r\n\
\r\n\
Let=E2=80=99s see if you get a notification."
                    .as_slice(),
                "Let’s see if you get a notification.",
            ),
            (
                b"----==_mimepart_example\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
For you, Alex =E2=80=94 related to your saved topic."
                    .as_slice(),
                "For you, Alex — related to your saved topic.",
            ),
        ];

        for (raw, expected) in samples {
            let snippet = snippet_from_partial(raw);
            assert_eq!(snippet, expected);
            assert!(!looks_like_mime_artifact(&snippet));
        }
    }

    #[test]
    fn snippets_clean_html_even_when_the_part_is_mislabeled_as_plain_text() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
<style>.hidden { display: none }</style><p>Your order&nbsp;is <strong>ready</strong>.</p>";

        assert_eq!(snippet_from_partial(raw), "Your order is ready.");
    }

    #[test]
    fn clean_snippets_normalize_whitespace_and_enforce_the_preview_limit() {
        let text = format!("<p>Hello   Tallinn</p>{}", " x".repeat(400));
        let snippet = clean_snippet(&text);

        assert!(snippet.starts_with("Hello Tallinn"));
        assert!(!snippet.contains('<'));
        assert_eq!(snippet.chars().count(), 240);
    }

    #[test]
    fn uses_the_message_date_when_it_is_valid() {
        let parsed = mailparse::parse_mail(
            b"Date: Tue, 10 Feb 2026 12:34:56 +0200\r\nSubject: hello\r\n\r\nHi",
        )
        .unwrap();
        let response = vec![
            "* 1 FETCH (UID 7 INTERNALDATE \"09-Feb-2026 07:16:47 +0100\" RFC822 {1}\r\n".into(),
        ];

        assert_eq!(
            message_received_at(&parsed, &response)
                .unwrap()
                .to_rfc3339(),
            "2026-02-10T10:34:56+00:00"
        );
    }

    #[test]
    fn uses_internal_date_when_the_message_date_is_missing_or_invalid() {
        for raw in [
            b"Subject: missing date\r\n\r\nHi".as_slice(),
            b"Date: definitely not a date\r\nSubject: invalid date\r\n\r\nHi".as_slice(),
        ] {
            let parsed = mailparse::parse_mail(raw).unwrap();
            let response = vec![
                "* 1 FETCH (UID 7 INTERNALDATE \"09-Feb-2026 07:16:47 +0100\" RFC822 {1}\r\n"
                    .into(),
            ];

            assert_eq!(
                message_received_at(&parsed, &response)
                    .unwrap()
                    .to_rfc3339(),
                "2026-02-09T06:16:47+00:00"
            );
        }
    }

    #[test]
    fn rejects_a_message_when_no_real_date_is_available() {
        let parsed = mailparse::parse_mail(b"Subject: no date\r\n\r\nHi").unwrap();
        let error =
            message_received_at(&parsed, &["* 1 FETCH (UID 7 RFC822 {1}\r\n".into()]).unwrap_err();

        assert!(error.to_string().contains("no valid Date header"));
        assert!(error.to_string().contains("valid INTERNALDATE"));
    }

    #[test]
    fn parses_move_destination_uid() {
        assert_eq!(
            parse_copy_uid(&["D0004 OK [COPYUID 4 91 203] Move completed\r\n".into()]),
            Some(203)
        );
    }

    #[test]
    fn quotes_imap_values() {
        assert_eq!(quote_imap("Drafts/Team"), "\"Drafts/Team\"");
    }

    #[test]
    fn extracts_html_and_resolves_cid_images() {
        let raw = concat!(
            "Content-Type: multipart/related; boundary=related\r\n",
            "\r\n",
            "--related\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><strong>Hello</strong><img src=\"CID:logo@example\"></body></html>\r\n",
            "--related\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <logo@example>\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "iVBORw0KGgo=\r\n",
            "--related--\r\n"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();
        let html = extract_html(&parsed).unwrap().unwrap();
        let resolved = resolve_inline_images(html, &parsed).unwrap();
        assert!(resolved.contains("<strong>Hello</strong>"));
        assert!(resolved.contains("src=\"data:image/png;base64,iVBORw0KGgo=\""));
    }

    #[test]
    fn html_only_messages_get_searchable_plain_text() {
        let raw = b"Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>Tallinn</b></p>";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let html = extract_html(&parsed).unwrap().unwrap();
        assert_eq!(
            mail_parser::decoders::html::html_to_text(&html).trim(),
            "Hello Tallinn"
        );
    }

    #[test]
    fn unnamed_inline_text_parts_remain_message_bodies() {
        let raw = concat!(
            "Content-Type: multipart/alternative; boundary=body\r\n",
            "\r\n",
            "--body\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Disposition: inline\r\n",
            "\r\n",
            "Plain sent body\r\n",
            "--body\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Disposition: inline\r\n",
            "\r\n",
            "<p>HTML sent body</p>\r\n",
            "--body--\r\n"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();

        assert_eq!(extract_text(&parsed).unwrap().trim(), "Plain sent body");
        assert_eq!(
            extract_html(&parsed).unwrap().unwrap().trim(),
            "<p>HTML sent body</p>"
        );
        assert!(!has_attachment(&parsed));
        assert!(extract_attachments(&parsed, "message-1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn named_inline_text_parts_remain_attachments() {
        let raw = concat!(
            "Content-Type: text/plain; charset=utf-8; name=notes.txt\r\n",
            "Content-Disposition: inline; filename=notes.txt\r\n",
            "\r\n",
            "Attached notes\r\n"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();

        assert!(extract_text(&parsed).unwrap().is_empty());
        let attachments = extract_attachments(&parsed, "message-1").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "notes.txt");
    }

    #[test]
    fn sends_rich_text_as_a_plain_and_html_multipart_alternative() {
        let account = Account {
            id: uuid::Uuid::new_v4(),
            email: "sender@example.com".into(),
            account_name: "Test account".into(),
            display_name: "Sender".into(),
            provider_id: "test".into(),
            auth: AccountAuth::Password {
                username: "sender@example.com".into(),
            },
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let draft = ComposeMessage {
            account_id: account.id,
            to: vec!["recipient@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Formatted message".into(),
            body_text: "Hello there".into(),
            body_html: Some("<p>Hello <strong>there</strong></p>".into()),
            in_reply_to: Some("<parent@example.com>".into()),
            references: Some("<root@example.com> <parent@example.com>".into()),
            attachments: vec![],
        };

        let source =
            String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted())
                .unwrap();
        assert!(source.contains("multipart/alternative"));
        assert!(source.contains("Content-Type: text/plain"));
        assert!(source.contains("Content-Type: text/html"));
        assert!(source.contains("Hello there"));
        assert!(source.contains("<p>Hello <strong>there</strong></p>"));
        assert!(source.contains("In-Reply-To: <parent@example.com>"));
        assert!(source.contains("References: <root@example.com> <parent@example.com>"));
    }

    #[test]
    fn builds_an_amazon_ses_mailto_unsubscribe_message() {
        let account = Account {
            id: uuid::Uuid::new_v4(),
            email: "sender@example.com".into(),
            account_name: "Test account".into(),
            display_name: "Sender".into(),
            provider_id: "test".into(),
            auth: AccountAuth::Password {
                username: "sender@example.com".into(),
            },
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let draft = ComposeMessage {
            account_id: account.id,
            to: vec![
                "unsubscribe-0101019f81db9add-24fdafe6-2373-4ce1-b33a-659d9fc35f3f-000000@us-west-2.amazonses.com".into(),
            ],
            cc: vec![],
            bcc: vec![],
            subject: "https://o.us-west-2.user-subscription.com/oc/subscription/token?t=recipient".into(),
            body_text: String::new(),
            body_html: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
        };

        let source =
            String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted())
                .unwrap();
        assert!(source.contains("us-west-2.amazonses.com"));
        assert!(source.contains("user-subscription.com"));
    }

    #[test]
    fn canonicalizes_threading_headers() {
        assert_eq!(
            canonical_message_ids(" <root@example.com>\t<reply@example.com> "),
            Some("<root@example.com> <reply@example.com>".into())
        );
        assert_eq!(canonical_message_ids("not-a-message-id"), None);
    }

    #[test]
    fn detects_idle_only_as_a_complete_capability() {
        assert!(supports_idle(&[
            "* CAPABILITY IMAP4rev1 IDLE UIDPLUS".into()
        ]));
        assert!(!supports_idle(
            &["* CAPABILITY IMAP4rev1 X-NOT-IDLE".into()]
        ));
    }

    #[test]
    fn idle_reconciliation_watchdog_stays_inside_delivery_target() {
        assert!(IMAP_IDLE_RENEWAL < Duration::from_secs(10));
    }

    #[test]
    fn parses_uidvalidity_from_select_responses() {
        assert_eq!(
            parse_uid_validity(&["* OK [UIDVALIDITY 987654] UIDs valid".into()]),
            Some(987654)
        );
        assert_eq!(parse_uid_validity(&["* 12 EXISTS".into()]), None);
    }

    #[test]
    fn parses_header_only_threading_backfill_payloads() {
        let headers = parse_threading_headers(
            b"Message-ID: <reply@example.com>\r\nIn-Reply-To: <root@example.com>\r\nReferences: <root@example.com> <parent@example.com>\r\n\r\n",
        )
        .unwrap();
        assert_eq!(headers.message_id.as_deref(), Some("<reply@example.com>"));
        assert_eq!(headers.in_reply_to.as_deref(), Some("<root@example.com>"));
        assert_eq!(
            headers.reference_ids.as_deref(),
            Some("<root@example.com> <parent@example.com>")
        );
    }

    #[test]
    fn syncs_trash_as_an_isolated_local_mailbox() {
        let mut account = Account {
            id: uuid::Uuid::new_v4(),
            email: "me@gmail.com".into(),
            account_name: "Gmail".into(),
            display_name: "Me".into(),
            provider_id: "gmail".into(),
            auth: AccountAuth::Password {
                username: "me@gmail.com".into(),
            },
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: Security::Tls,
            archive_mailbox: "[Gmail]/All Mail".into(),
            spam_mailbox: "[Gmail]/Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let plans = mailbox_plans(&account);
        assert!(plans
            .iter()
            .any(|plan| plan.remote == "[Gmail]/Trash" && plan.local == "Trash"));
        account.provider_id = "custom".into();
        assert!(mailbox_plans(&account)
            .iter()
            .any(|plan| plan.remote == "Trash" && plan.local == "Trash"));
    }

    #[test]
    fn discovers_every_special_use_sent_folder_with_distinct_uid_storage() {
        let plans = vec![
            MailboxPlan::new("INBOX", "INBOX"),
            MailboxPlan::new("Sent", "Sent"),
        ];
        let listing = vec![
            r#"* LIST (\HasNoChildren \Sent) "/" "Sent Messages""#.into(),
            r#"* LIST (\HasNoChildren \Sent) "/" Sent"#.into(),
        ];

        let plans = resolve_special_mailboxes(plans, &listing);
        assert!(plans.iter().any(|plan| {
            plan.remote == "Sent" && plan.local == "Sent" && plan.storage == "Sent"
        }));
        assert!(plans.iter().any(|plan| {
            plan.remote == "Sent Messages"
                && plan.local == "Sent"
                && plan.storage == "Sent::Sent Messages"
        }));
    }

    #[test]
    fn parses_ordered_list_unsubscribe_urls() {
        let urls = parse_list_urls(
            " <mailto:list@example.com?subject=unsubscribe>, <https://example.com/unsubscribe/token>",
        );
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].scheme(), "mailto");
        assert_eq!(urls[1].as_str(), "https://example.com/unsubscribe/token");
    }

    #[test]
    fn parses_rfc_2369_comments_and_ignores_invalid_entries() {
        let urls = parse_list_urls(
            "(Use this command) <not a URL>, (preferred) <mailto:list@example.com?body=unsubscribe%20news>, <https://example.com/unsubscribe>",
        );
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].scheme(), "mailto");
        assert_eq!(urls[1].scheme(), "https");
    }

    #[test]
    fn ignores_malformed_list_unsubscribe_values() {
        assert!(parse_list_urls("https://example.com/unsubscribe").is_empty());
        assert!(parse_list_urls("<https://example.com").is_empty());
    }

    #[test]
    fn parses_mailto_subject_body_and_literal_plus_signs() {
        let url = Url::parse(
            "mailto:list%2Bnews@example.com?subject=unsubscribe%2Bweekly&body=stop%20news",
        )
        .unwrap();
        assert_eq!(
            parse_mailto_action(&url).unwrap(),
            MailtoAction {
                to: "list+news@example.com".into(),
                subject: "unsubscribe+weekly".into(),
                body: "stop news".into(),
            }
        );
    }

    #[test]
    fn parses_mailto_query_recipient_and_uses_first_singleton_field() {
        let url =
            Url::parse("mailto:?to=list@example.com&subject=first&subject=second&body=unsubscribe")
                .unwrap();
        assert_eq!(
            parse_mailto_action(&url).unwrap(),
            MailtoAction {
                to: "list@example.com".into(),
                subject: "first".into(),
                body: "unsubscribe".into(),
            }
        );
    }

    #[test]
    fn rejects_ambiguous_or_dangerous_mailto_actions() {
        for value in [
            "mailto:first@example.com,second@example.com",
            "mailto:first@example.com?to=second@example.com",
            "mailto:list@example.com?subject=unsubscribe%0D%0ABcc%3Avictim@example.com",
            "mailto:",
        ] {
            let url = Url::parse(value).unwrap();
            assert!(
                parse_mailto_action(&url).is_err(),
                "{value} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_valid_long_provider_tokens_without_weakening_address_syntax() {
        assert!(parse_recipient_mailbox(
            "unsubscribe-0101019f81db9add-24fdafe6-2373-4ce1-b33a-659d9fc35f3f-000000@us-west-2.amazonses.com"
        )
        .is_ok());
        assert!(parse_recipient_mailbox(
            "unsubscribe-012345678901234567890123456789012345678901234567890123456789@unsubscribe-eu.customer.io"
        )
        .is_ok());
        assert!(parse_recipient_mailbox(
            "01234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123@unsub.beehiiv.com"
        )
        .is_ok());
        assert!(parse_recipient_mailbox(
            "unsubscribe-0101019f81db9add-24fdafe6-2373-4ce1-b33a-659d9fc35f3f-000000\r\nBcc:victim@example.com@us-west-2.amazonses.com"
        )
        .is_err());
        assert!(parse_recipient_mailbox(
            ".unsubscribe-0101019f81db9add-24fdafe6-2373-4ce1-b33a-659d9fc35f3f-000000@example.com"
        )
        .is_err());
    }

    #[test]
    fn validates_one_click_urls_before_exposing_the_action() {
        assert!(is_safe_one_click_url(
            &Url::parse("https://unsubscribe.example/token").unwrap()
        ));
        for value in [
            "http://unsubscribe.example/token",
            "https://user@unsubscribe.example/token",
            "https://localhost/unsubscribe",
            "https://127.0.0.1/unsubscribe",
            "https://[::ffff:127.0.0.1]/unsubscribe",
            "mailto:list@example.com",
        ] {
            assert!(
                !is_safe_one_click_url(&Url::parse(value).unwrap()),
                "{value} should not be eligible for one-click POST"
            );
        }
    }

    #[tokio::test]
    async fn skips_invalid_actions_and_uses_the_next_safe_fallback() {
        let raw = concat!(
            "List-Unsubscribe: (broken) <mailto:first@example.com,second@example.com>, <https://example.com/unsubscribe>\r\n",
            "\r\n",
            "Hello"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();
        assert_eq!(
            extract_unsubscribe(&parsed, raw.as_bytes(), None).await,
            (
                Some("web".into()),
                Some("https://example.com/unsubscribe".into())
            )
        );
    }

    #[tokio::test]
    async fn falls_back_to_a_web_link_without_verified_dkim() {
        let raw = concat!(
            "List-Unsubscribe: <mailto:list@example.com?subject=unsubscribe>, <https://example.com/unsubscribe/token>\r\n",
            "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
            "\r\n",
            "Hello"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();
        assert_eq!(
            extract_unsubscribe(&parsed, raw.as_bytes(), None).await,
            (
                Some("mailto".into()),
                Some("mailto:list@example.com?subject=unsubscribe".into())
            )
        );
    }

    #[test]
    fn rejects_private_unsubscribe_destinations() {
        for value in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "100::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(
                !is_public_address(value.parse().unwrap()),
                "{value} must not be reachable by unsubscribe POST"
            );
        }
        for value in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                is_public_address(value.parse().unwrap()),
                "{value} should be treated as public"
            );
        }
    }

    #[test]
    fn extracts_and_sanitizes_attachment_metadata() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=mixed\r\n",
            "\r\n",
            "--mixed\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\nHello\r\n",
            "--mixed\r\n",
            "Content-Type: application/pdf; name=ignored.pdf\r\n",
            "Content-Disposition: attachment; filename=../../invoice.pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "cGRm\r\n",
            "--mixed--\r\n"
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).unwrap();
        let attachments = extract_attachments(&parsed, "message-1").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "invoice.pdf");
        assert_eq!(attachments[0].attachment.mime_type, "application/pdf");
        assert_eq!(attachments[0].bytes, b"pdf");
    }

    #[test]
    fn flags_executable_attachments_without_blocking_a_save() {
        assert!(is_potentially_unsafe("invoice.command", "text/plain"));
        assert!(!is_potentially_unsafe("invoice.pdf", "application/pdf"));
    }

    #[test]
    fn extracts_safe_structural_classification_signals() {
        let raw = b"List-Unsubscribe: <https://example.test/unsubscribe?recipient=private>\r\nList-Id: <product.example.test>\r\nPrecedence: bulk\r\nAuto-Submitted: auto-generated\r\nReply-To: support@example.test\r\n\r\nHello";
        let parsed = mailparse::parse_mail(raw).unwrap();
        let signals = classification_signals(&parsed, &[]);

        assert!(signals.contains("Mailing-list unsubscribe header present"));
        assert!(signals.contains("Mailing-list identifier header present"));
        assert!(signals.contains("Bulk-mail precedence header"));
        assert!(signals.contains("Automatically generated message header"));
        assert!(signals.contains("Reply-To header present"));
        assert!(!signals.contains("recipient=private"));
    }

    #[test]
    fn records_only_gmail_category_metadata() {
        let parsed = mailparse::parse_mail(b"Subject: hello\r\n\r\nHi").unwrap();
        let signals = classification_signals(
            &parsed,
            &["* 1 FETCH (X-GM-LABELS (\\Inbox \\Category_Promotions))".into()],
        );

        assert!(signals.contains("Gmail category: Promotions"));
        assert!(!signals.contains("\\Inbox"));
    }
}
