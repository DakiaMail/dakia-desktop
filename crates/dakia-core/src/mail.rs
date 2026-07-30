#[cfg(test)]
use crate::storage::ThreadingHeaders;
use crate::{
    flowed::decode_format_flowed,
    mime_budget::{
        preflight_raw_message, validate_header_bytes, validate_structure, MAX_RAW_MESSAGE_BYTES,
    },
    oauth::OAuthTokens,
    provider::Security,
    storage::{stable_message_id, Attachment, AttachmentData, AttachmentPresentation, MailSummary},
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
use mail_parser::{
    decoders::{
        base64::base64_decode, charsets::map::charset_decoder,
        quoted_printable::quoted_printable_decode,
    },
    Address as ParsedAddress, HeaderForm, HeaderName, HeaderValue, Message as ParsedMessage,
    MessageParser, MessagePart, MimeHeaders, PartType,
};
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
const MAX_ATTACHMENT_TOTAL_BYTES: usize = 50 * 1024 * 1024;
const MAX_ATTACHMENT_COUNT: usize = 50;
const MIME_CONTENT_UNDECODABLE: &str = "mime_content_undecodable";
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
        let highest_processed_uid = uids.iter().copied().max();
        for uid in uids {
            let fields = "FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE LIST-ID PRECEDENCE AUTO-SUBMITTED)]";
            let response = client
                .command_with_literal(&format!("UID FETCH {uid} ({fields})"))
                .await?;
            if let Some(raw) = response.literal {
                match parse_header_message(account, "INBOX", uid, &response.lines, &raw) {
                    Ok(message) => messages.push(message),
                    Err(error) => {
                        eprintln!("Dakia rejected INBOX UID {uid} before persistence: {error}")
                    }
                }
            }
        }
        let new_messages = self
            .store
            .save_synced_messages(account.id, "INBOX", &messages)
            .await?;
        if let Some(uid) = highest_processed_uid {
            self.store
                .advance_mailbox_sync_watermark(account.id, "INBOX", uid)
                .await?;
        }
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
                            match parse_catalog_message(
                                account,
                                &item.plan.storage,
                                *uid,
                                &response.lines,
                                &headers,
                                snippet,
                            ) {
                                Ok(message) => messages.push(message),
                                Err(error) => eprintln!(
                                    "Dakia rejected {} UID {} before persistence: {error}",
                                    item.plan.storage, uid
                                ),
                            }
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

    /// Fetches the original RFC 822 bytes for an explicit export without
    /// changing the message's `\\Seen` flag or retaining the bytes locally.
    ///
    /// The local mailbox catalogue remains the locator authority: its saved
    /// UIDVALIDITY must still match the selected remote mailbox before a UID
    /// can be used. This prevents a recycled UID from exporting a different
    /// message after a mailbox rebuild.
    pub async fn fetch_raw_message(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
    ) -> Result<Vec<u8>> {
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
            bail!("mailbox identity changed; sync the account before exporting this message");
        }
        let response = client
            .command_with_literal(&raw_message_fetch_command(uid))
            .await?;
        let raw = response.literal.context("message is no longer available")?;
        let _ = client.command("LOGOUT").await;
        Ok(raw)
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
                        let Ok(message) = parse_catalog_message(
                            account,
                            &plan.storage,
                            uid,
                            &response.lines,
                            &headers,
                            snippet,
                        ) else {
                            continue;
                        };
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

fn raw_message_fetch_command(uid: u32) -> String {
    format!("UID FETCH {uid} (BODY.PEEK[])")
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
                if length > MAX_RAW_MESSAGE_BYTES {
                    bail!("mime_raw_message_too_large");
                }
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

fn configured_message_parser() -> MessageParser {
    MessageParser::new()
        .with_minimal_headers()
        .with_message_ids()
        .header_text(HeaderName::Other("List-Unsubscribe-Post".into()))
        .header_text(HeaderName::Other("Precedence".into()))
        .header_text(HeaderName::Other("Auto-Submitted".into()))
}

fn parse_complete_message(raw: &[u8]) -> Result<ParsedMessage<'_>> {
    preflight_raw_message(raw)?;
    let parser = configured_message_parser();
    let message = if uses_cr_only_headers(raw) {
        // Some providers still emit CR-only line endings. Normalize only that
        // nonstandard form for MIME parsing; DKIM verification retains `raw`.
        let normalized = raw
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if *byte == b'\r' && raw.get(index + 1) != Some(&b'\n') {
                    b'\n'
                } else {
                    *byte
                }
            })
            .collect::<Vec<_>>();
        parser.parse(&normalized).map(ParsedMessage::into_owned)
    } else {
        parser.parse(raw)
    }
    .context("message parser could not find RFC 5322 headers")?;
    validate_message_budget(&message)?;
    Ok(message)
}

fn parse_header_block(raw: &[u8]) -> Result<ParsedMessage<'_>> {
    preflight_raw_message(raw)?;
    let parser = configured_message_parser();
    let message = if uses_cr_only_headers(raw) {
        let normalized = raw
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if *byte == b'\r' && raw.get(index + 1) != Some(&b'\n') {
                    b'\n'
                } else {
                    *byte
                }
            })
            .collect::<Vec<_>>();
        parser
            .parse_headers(&normalized)
            .map(ParsedMessage::into_owned)
    } else {
        parser.parse_headers(raw)
    }
    .context("message parser could not find RFC 5322 headers")?;
    validate_message_budget(&message)?;
    Ok(message)
}

fn uses_cr_only_headers(raw: &[u8]) -> bool {
    let cr_separator = raw.windows(2).position(|pair| pair == b"\r\r");
    let first_lf = raw.iter().position(|byte| *byte == b'\n');
    cr_separator.is_some_and(|separator| first_lf.is_none_or(|lf| separator < lf))
}

fn validate_message_budget(message: &ParsedMessage<'_>) -> Result<()> {
    let mut part_count = 0;
    let mut header_bytes = 0;
    let mut multipart_depth = 0;
    let mut pending = vec![(message, 0_u32, 0_usize)];
    while let Some((current_message, part_id, depth)) = pending.pop() {
        let Some(part) = current_message.part(part_id) else {
            continue;
        };
        part_count += 1;
        header_bytes += part
            .raw_body_offset()
            .saturating_sub(part.raw_header_offset()) as usize;
        match &part.body {
            PartType::Multipart(children) => {
                let child_depth = depth + 1;
                multipart_depth = multipart_depth.max(child_depth);
                pending.extend(
                    children
                        .iter()
                        .rev()
                        .map(|child| (current_message, *child, child_depth)),
                );
            }
            PartType::Message(nested) if !nested.parts.is_empty() => {
                pending.push((nested, 0, depth + 1));
            }
            _ => {}
        }
    }
    validate_header_bytes(header_bytes)?;
    validate_structure(part_count, multipart_depth)?;
    Ok(())
}

fn decoded_header(message: &ParsedMessage<'_>, name: &str) -> String {
    message
        .header_as(name, HeaderForm::Text)
        .into_iter()
        .find_map(|value| value.into_text())
        .map(|value| value.into_owned())
        .unwrap_or_default()
}

fn header_values(message: &ParsedMessage<'_>, name: &str) -> Vec<String> {
    message
        .headers_raw()
        .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.replace(['\r', '\n'], " ").trim().to_owned())
        .collect()
}

fn mime_type(part: &MessagePart<'_>) -> String {
    part.content_type()
        .and_then(|content_type| {
            content_type
                .subtype()
                .map(|subtype| format!("{}/{}", content_type.ctype(), subtype))
        })
        .map(|value| safe_mime_type(&value))
        .unwrap_or_else(|| {
            if part.is_text_html() {
                "text/html".into()
            } else if part.is_text() {
                "text/plain".into()
            } else {
                "application/octet-stream".into()
            }
        })
}

fn flowed_text(part: &MessagePart<'_>, text: &str) -> String {
    let Some(content_type) = part.content_type() else {
        return text.to_owned();
    };
    if !content_type
        .attribute("format")
        .is_some_and(|format| format.eq_ignore_ascii_case("flowed"))
    {
        return text.to_owned();
    }
    let delsp = content_type
        .attribute("delsp")
        .is_some_and(|value| value.eq_ignore_ascii_case("yes"));
    decode_format_flowed(text, delsp)
}

fn has_supported_text_charset(part: &MessagePart<'_>) -> bool {
    let Some(charset) = part
        .content_type()
        .and_then(|content_type| mime_attribute(content_type, "charset"))
    else {
        return true;
    };
    charset_decoder(charset.trim().as_bytes()).is_some()
}

fn message_received_at(
    parsed: &ParsedMessage<'_>,
    response_lines: &[String],
) -> Result<chrono::DateTime<Utc>> {
    if let Some(date) = parsed
        .header_values(HeaderName::Date)
        .find_map(HeaderValue::as_datetime)
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
    let parsed = parse_complete_message(raw)?;
    let header = |name| decoded_header(&parsed, name);
    let received_at = message_received_at(&parsed, response_lines)?;
    let from_header = header("From");
    let (from_name, from_address) = parse_first_address(&from_header);
    let id = stable_message_id(account.id, mailbox, uid);
    // Classification must see the selected HTML before CID/Content-Location
    // URLs are rewritten to data URLs. MIME transport-inline is not the same
    // as a user-facing attachment.
    let selected_bodies = select_bodies(&parsed)?;
    let selected_html = selected_bodies.html;
    let attachments = extract_attachments_from_raw(&parsed, raw, &id, selected_html.as_deref())?;
    let body_html = selected_html
        .map(|html| resolve_inline_images_from_raw(html, &parsed, raw))
        .transpose()?;
    let mut body_text = selected_bodies.text;
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
        has_attachments: !attachments.is_empty(),
        attachments,
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
    let parsed = parse_header_block(raw_headers)?;
    let header = |name| decoded_header(&parsed, name);
    let received_at = message_received_at(&parsed, response_lines)?;
    let (from_name, from_address) = parse_first_address(&header("From"));
    let id = stable_message_id(account.id, mailbox, uid);
    let flags = response_lines.join(" ");
    let structure = flags.to_ascii_lowercase();
    // A header-only BODYSTRUCTURE cannot tell whether a named inline part is
    // a referenced signature asset. Only an explicit attachment disposition
    // is safe to surface provisionally; complete MIME parsing supplies the
    // authoritative user-facing attachment state.
    let has_attachments = structure.contains("attachment");
    let unsubscribe_url = header_values(&parsed, "List-Unsubscribe")
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
        // Header and preview fetches never contain authoritative MIME part
        // classification or full bodies. Keeping this distinct prevents a
        // later catalogue refresh from replacing durable starred content.
        content_state: "headers_only".into(),
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
    partial_text_parts(raw)
        .into_iter()
        .filter_map(|part| parsed_snippet(part.as_bytes()))
        .find(|snippet| !looks_like_mime_artifact(snippet))
        .or_else(|| parsed_snippet(raw).filter(|snippet| !looks_like_mime_artifact(snippet)))
        .map(|text| clean_snippet(&text))
        .unwrap_or_default()
}

fn parsed_snippet(raw: &[u8]) -> Option<String> {
    let parsed = parse_complete_message(raw).ok()?;
    let plain = extract_text(&parsed);
    if !plain.trim().is_empty() {
        return Some(plain);
    }
    extract_html(&parsed).map(|html| mail_parser::decoders::html::html_to_text(&html))
}

/// A partial RFC822 fetch often ends before a multipart message's closing
/// boundary. A complete parser may not have enough structure to select a body,
/// but the first text leaf can still be complete enough to decode. Extract only MIME
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
    let parsed = parse_header_block(raw_headers)?;
    let header = |name| decoded_header(&parsed, name);
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
    if !value.contains('<') || !value.contains('>') {
        return None;
    }
    let raw = format!("Message-ID: {value}\r\n\r\n");
    let message = parse_header_block(raw.as_bytes()).ok()?;
    let ids = message
        .header(HeaderName::MessageId)
        .and_then(HeaderValue::as_text_list)?;
    (!ids.is_empty()).then(|| {
        ids.iter()
            .map(|id| format!("<{id}>"))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

#[cfg(test)]
fn parse_threading_headers(raw: &[u8]) -> Result<ThreadingHeaders> {
    let parsed = parse_header_block(raw)?;
    let header = |name| decoded_header(&parsed, name);
    Ok(ThreadingHeaders {
        message_id: canonical_message_ids(&header("Message-ID")),
        in_reply_to: canonical_message_ids(&header("In-Reply-To")),
        reference_ids: canonical_message_ids(&header("References")),
    })
}

async fn extract_unsubscribe(
    parsed: &ParsedMessage<'_>,
    raw: &[u8],
    dkim_authenticator: Option<&MessageAuthenticator>,
) -> (Option<String>, Option<String>) {
    let unsubscribe_headers = header_values(parsed, "List-Unsubscribe");
    let urls = unsubscribe_headers
        .iter()
        .flat_map(|value| parse_list_urls(value))
        .collect::<Vec<_>>();
    if urls.is_empty() {
        return (None, None);
    }

    let post_headers = header_values(parsed, "List-Unsubscribe-Post");
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
fn classification_signals(mail: &ParsedMessage<'_>, response_lines: &[String]) -> String {
    let mut signals = Vec::new();
    if !header_values(mail, "List-Unsubscribe").is_empty() {
        signals.push("Mailing-list unsubscribe header present");
    }
    if !header_values(mail, "List-Id").is_empty() {
        signals.push("Mailing-list identifier header present");
    }
    if let Some(value) = header_values(mail, "Precedence").into_iter().next() {
        signals.push(match value.trim().to_ascii_lowercase().as_str() {
            "bulk" => "Bulk-mail precedence header",
            "list" => "Mailing-list precedence header",
            _ => "Precedence header present",
        });
    }
    if let Some(value) = header_values(mail, "Auto-Submitted").into_iter().next() {
        signals.push(match value.trim().to_ascii_lowercase().as_str() {
            "auto-generated" => "Automatically generated message header",
            "auto-replied" => "Automatically replied message header",
            _ => "Auto-Submitted header present",
        });
    }
    if !decoded_header(mail, "In-Reply-To").is_empty() {
        signals.push("Reply thread header present");
    }
    if !decoded_header(mail, "Reply-To").is_empty() {
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
    let raw = format!("From: {value}\r\n\r\n");
    let Some(parsed) = parse_header_block(raw.as_bytes()).ok() else {
        return (None, value.to_owned());
    };
    match parsed
        .header(HeaderName::From)
        .and_then(HeaderValue::as_address)
    {
        Some(ParsedAddress::List(addresses)) => addresses
            .first()
            .and_then(|address| {
                address.address.as_ref().map(|email| {
                    (
                        address.name.as_ref().map(ToString::to_string),
                        email.to_string(),
                    )
                })
            })
            .unwrap_or((None, value.to_owned())),
        Some(ParsedAddress::Group(groups)) => groups
            .iter()
            .flat_map(|group| group.addresses.iter())
            .find_map(|address| {
                address.address.as_ref().map(|email| {
                    (
                        address.name.as_ref().map(ToString::to_string),
                        email.to_string(),
                    )
                })
            })
            .unwrap_or((None, value.to_owned())),
        None => (None, value.to_owned()),
    }
}

#[derive(Default)]
struct BodySelection {
    plain: Vec<String>,
    html: Vec<String>,
    valid_candidates: usize,
    undecodable_candidates: usize,
}

impl BodySelection {
    fn merge(&mut self, mut other: Self) {
        self.plain.append(&mut other.plain);
        self.html.append(&mut other.html);
        self.valid_candidates += other.valid_candidates;
        self.undecodable_candidates += other.undecodable_candidates;
    }

    fn into_body_content(self) -> Result<SelectedBodyContent> {
        if self.valid_candidates == 0 && self.undecodable_candidates > 0 {
            bail!("{MIME_CONTENT_UNDECODABLE}");
        }
        Ok(SelectedBodyContent {
            text: self
                .plain
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            html: {
                let html = self
                    .html
                    .into_iter()
                    .filter(|value| !value.trim().is_empty())
                    .collect::<Vec<_>>();
                (!html.is_empty()).then(|| html.join("\n"))
            },
        })
    }
}

struct SelectedBodyContent {
    text: String,
    html: Option<String>,
}

fn select_bodies(mail: &ParsedMessage<'_>) -> Result<SelectedBodyContent> {
    select_body_part(mail, 0).into_body_content()
}

fn select_body_part(mail: &ParsedMessage<'_>, part_id: u32) -> BodySelection {
    let Some(part) = mail.part(part_id) else {
        return BodySelection::default();
    };
    if is_attachment_part(mail, part) {
        return BodySelection::default();
    }
    match &part.body {
        PartType::Multipart(children) => {
            let subtype = part
                .content_type()
                .and_then(|value| value.subtype())
                .unwrap_or_default();
            if subtype.eq_ignore_ascii_case("alternative") {
                select_alternative(mail, children)
            } else if subtype.eq_ignore_ascii_case("related") {
                select_related(mail, part, children)
            } else {
                let mut selected = BodySelection::default();
                for child in children {
                    selected.merge(select_body_part(mail, *child));
                }
                selected
            }
        }
        PartType::Message(_) => BodySelection::default(),
        PartType::Text(_) | PartType::Html(_) => select_text_leaf(mail, part),
        PartType::Binary(_) | PartType::InlineBinary(_) => BodySelection::default(),
    }
}

fn select_alternative(mail: &ParsedMessage<'_>, children: &[u32]) -> BodySelection {
    let mut selected = BodySelection::default();
    let mut chosen_plain = None;
    let mut chosen_html = None;
    for child in children {
        let candidate = select_body_part(mail, *child);
        // RFC multipart/alternative orders representations by increasing
        // faithfulness. Keep only the last supported representation of each
        // kind; never concatenate competing alternatives.
        if !candidate.plain.is_empty() {
            chosen_plain = Some(candidate.plain.join("\n\n"));
        }
        if !candidate.html.is_empty() {
            chosen_html = Some(candidate.html.join("\n"));
        }
        selected.valid_candidates += candidate.valid_candidates;
        selected.undecodable_candidates += candidate.undecodable_candidates;
    }
    selected.plain.extend(chosen_plain);
    selected.html.extend(chosen_html);
    selected
}

fn select_related(
    mail: &ParsedMessage<'_>,
    container: &MessagePart<'_>,
    children: &[u32],
) -> BodySelection {
    let declared_start = container
        .content_type()
        .and_then(|content_type| mime_attribute(content_type, "start"))
        .map(normalized_content_reference);
    let root = declared_start
        .as_deref()
        .and_then(|start| {
            children.iter().copied().find(|child| {
                mail.part(*child)
                    .and_then(MessagePart::content_id)
                    .map(normalized_content_reference)
                    .is_some_and(|content_id| content_id == start)
            })
        })
        .or_else(|| children.first().copied());
    root.map(|part_id| select_body_part(mail, part_id))
        .unwrap_or_default()
}

fn normalized_content_reference(value: &str) -> String {
    value.trim().trim_matches(['<', '>']).to_ascii_lowercase()
}

fn mime_attribute<'a>(
    content_type: &'a mail_parser::ContentType<'a>,
    name: &str,
) -> Option<&'a str> {
    content_type
        .attributes()?
        .iter()
        .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
        .map(|attribute| attribute.value.as_ref())
}

fn select_text_leaf(mail: &ParsedMessage<'_>, part: &MessagePart<'_>) -> BodySelection {
    let content_type = mime_type(part);
    if !matches!(content_type.as_str(), "text/plain" | "text/html")
        || is_attachment_part(mail, part)
    {
        return BodySelection::default();
    }
    let mut selected = BodySelection::default();
    match decode_text_candidate(mail, part) {
        Ok(text) => {
            selected.valid_candidates = 1;
            if part.is_text_html() {
                selected.html.push(text);
            } else {
                selected.plain.push(flowed_text(part, &text));
            }
        }
        Err(()) => selected.undecodable_candidates = 1,
    }
    selected
}

fn decode_text_candidate(
    mail: &ParsedMessage<'_>,
    part: &MessagePart<'_>,
) -> std::result::Result<String, ()> {
    if !has_supported_transfer_encoding(part) {
        return Err(());
    }
    let decoded_transport = decoded_part_bytes(mail.raw_message(), part).ok_or(())?;
    let declared_charset = part
        .content_type()
        .and_then(|content_type| mime_attribute(content_type, "charset"));
    if declared_charset.is_some_and(|_| !has_supported_text_charset(part)) {
        return std::str::from_utf8(&decoded_transport)
            .map(ToOwned::to_owned)
            .map_err(|_| ());
    }
    if part.is_encoding_problem {
        return Err(());
    }
    part.text_contents().map(ToOwned::to_owned).ok_or(())
}

fn has_supported_transfer_encoding(part: &MessagePart<'_>) -> bool {
    part.content_transfer_encoding().is_none_or(|encoding| {
        matches!(
            encoding.trim().to_ascii_lowercase().as_str(),
            "7bit" | "8bit" | "binary" | "base64" | "quoted-printable"
        )
    })
}

fn decoded_part_bytes(raw_message: &[u8], part: &MessagePart<'_>) -> Option<Vec<u8>> {
    let raw = raw_message.get(part.raw_body_offset() as usize..part.raw_end_offset() as usize)?;
    match part
        .content_transfer_encoding()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("base64") => base64_decode(raw),
        Some("quoted-printable") => quoted_printable_decode(raw),
        Some("7bit" | "8bit" | "binary") | None => Some(raw.to_vec()),
        Some(_) => None,
    }
}

fn extract_text(mail: &ParsedMessage<'_>) -> String {
    select_bodies(mail)
        .map(|body| body.text)
        .unwrap_or_default()
}

fn extract_html(mail: &ParsedMessage<'_>) -> Option<String> {
    select_bodies(mail).ok().and_then(|body| body.html)
}

#[cfg(test)]
fn resolve_inline_images(html: String, mail: &ParsedMessage<'_>) -> Result<String> {
    resolve_inline_images_from_raw(html, mail, mail.raw_message())
}

fn resolve_inline_images_from_raw(
    mut html: String,
    mail: &ParsedMessage<'_>,
    raw: &[u8],
) -> Result<String> {
    let lower_html = html.to_ascii_lowercase();
    let inline_images = collect_inline_images(mail, raw, &lower_html);
    for (reference, data_url) in inline_images {
        html = replace_resource_reference(&html, &reference, &data_url)?;
    }
    Ok(html)
}

fn collect_inline_images(
    mail: &ParsedMessage<'_>,
    raw: &[u8],
    lower_html: &str,
) -> Vec<(String, String)> {
    attachment_part_ids(mail)
        .into_iter()
        .filter_map(|part_id| mail.part(part_id))
        .filter(|part| {
            !part.is_encoding_problem && part.is_binary() && has_supported_transfer_encoding(part)
        })
        .filter_map(|part| {
            let mime_type = mime_type(part);
            if !mime_type.starts_with("image/") {
                return None;
            }
            let references = [
                part.content_id()
                    .map(|value| format!("cid:{}", value.trim().trim_matches(['<', '>']))),
                part.content_location().map(ToOwned::to_owned),
            ]
            .into_iter()
            .flatten()
            .filter(|reference| {
                !reference.is_empty() && lower_html.contains(&reference.to_ascii_lowercase())
            })
            .collect::<Vec<_>>();
            if references.is_empty() {
                return None;
            }
            let encoded = STANDARD.encode(decoded_part_bytes(raw, part)?);
            let data_url = format!("data:{mime_type};base64,{encoded}");
            Some(
                references
                    .into_iter()
                    .map(|reference| (reference, data_url.clone()))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn replace_resource_reference(input: &str, reference: &str, replacement: &str) -> Result<String> {
    let mut output = input.to_owned();
    for (needle, replacement) in [
        (format!("\"{reference}\""), format!("\"{replacement}\"")),
        (format!("'{reference}'"), format!("'{replacement}'")),
        (format!("url({reference})"), format!("url({replacement})")),
    ] {
        output = replace_ascii_case_insensitive_bounded(&output, &needle, &replacement)?;
    }
    Ok(output)
}

fn replace_ascii_case_insensitive_bounded(
    input: &str,
    needle: &str,
    replacement: &str,
) -> Result<String> {
    if needle.is_empty() {
        return Ok(input.to_owned());
    }
    let lower_input = input.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let occurrences = lower_input.match_indices(&lower_needle).count();
    let projected_len = input
        .len()
        .checked_add(
            replacement
                .len()
                .saturating_sub(needle.len())
                .checked_mul(occurrences)
                .context("mime_resolved_html_too_large")?,
        )
        .context("mime_resolved_html_too_large")?;
    if projected_len > MAX_RAW_MESSAGE_BYTES {
        bail!("mime_resolved_html_too_large");
    }
    let mut output = String::with_capacity(projected_len);
    let mut cursor = 0;
    while let Some(offset) = lower_input[cursor..].find(&lower_needle) {
        let start = cursor + offset;
        output.push_str(&input[cursor..start]);
        output.push_str(replacement);
        cursor = start + needle.len();
    }
    output.push_str(&input[cursor..]);
    Ok(output)
}

fn supplied_attachment_name(mail: &ParsedMessage<'_>, part: &MessagePart<'_>) -> Option<String> {
    [
        raw_parameter_value(
            mail,
            part,
            HeaderName::ContentDisposition,
            "Content-Disposition",
            "attachment",
            "filename",
            true,
        ),
        raw_parameter_value(
            mail,
            part,
            HeaderName::ContentDisposition,
            "Content-Disposition",
            "attachment",
            "filename",
            false,
        ),
        raw_parameter_value(
            mail,
            part,
            HeaderName::ContentType,
            "Content-Type",
            "application/octet-stream",
            "name",
            true,
        ),
        raw_parameter_value(
            mail,
            part,
            HeaderName::ContentType,
            "Content-Type",
            "application/octet-stream",
            "name",
            false,
        ),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .or_else(|| {
        part.attachment_name()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    })
}

fn raw_parameter_value(
    mail: &ParsedMessage<'_>,
    part: &MessagePart<'_>,
    header_name: HeaderName<'_>,
    header_label: &str,
    header_base: &str,
    parameter: &str,
    extended: bool,
) -> Option<String> {
    let header = part
        .headers
        .iter()
        .find(|header| header.name == header_name)?;
    let raw_value = std::str::from_utf8(
        mail.raw_message()
            .get(header.offset_start as usize..header.offset_end as usize)?,
    )
    .ok()?;
    let parameters = split_mime_parameters(raw_value);
    let parameter_lower = parameter.to_ascii_lowercase();
    let selected = if extended {
        let standalone = format!("{parameter_lower}*");
        let standalone = parameters
            .iter()
            .find(|(name, value, _)| {
                name.eq_ignore_ascii_case(&standalone) && valid_percent_escapes(value)
            })
            .map(|(_, _, raw)| vec![raw.as_str()]);
        standalone.or_else(|| {
            let continued = parameters
                .iter()
                .filter(|(name, value, _)| {
                    let suffix = name
                        .to_ascii_lowercase()
                        .strip_prefix(&parameter_lower)
                        .unwrap_or_default()
                        .to_owned();
                    suffix.starts_with('*')
                        && suffix[1..]
                            .chars()
                            .next()
                            .is_some_and(|ch| ch.is_ascii_digit())
                        && valid_percent_escapes(value)
                })
                .map(|(_, _, raw)| raw.as_str())
                .collect::<Vec<_>>();
            (!continued.is_empty()).then_some(continued)
        })?
    } else {
        vec![parameters
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(&parameter_lower))?
            .2
            .as_str()]
    };
    let raw = format!(
        "{header_label}: {header_base}; {}\r\n\r\n",
        selected.join("; ")
    );
    let parsed = configured_message_parser().parse_headers(raw.as_bytes())?;
    let parsed_part = parsed.part(0)?;
    let value = match header_name {
        HeaderName::ContentDisposition => parsed_part
            .content_disposition()
            .and_then(|content_type| mime_attribute(content_type, parameter)),
        HeaderName::ContentType => parsed_part
            .content_type()
            .and_then(|content_type| mime_attribute(content_type, parameter)),
        _ => None,
    }?;
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn split_mime_parameters(value: &str) -> Vec<(String, String, String)> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                if start > 0 {
                    push_mime_parameter(&mut tokens, &value[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if start > 0 {
        push_mime_parameter(&mut tokens, &value[start..]);
    }
    tokens
}

fn push_mime_parameter(parameters: &mut Vec<(String, String, String)>, token: &str) {
    let raw = token.trim();
    let Some((name, value)) = raw.split_once('=') else {
        return;
    };
    parameters.push((
        name.trim().to_ascii_lowercase(),
        value.trim().trim_matches('"').to_owned(),
        raw.to_owned(),
    ));
}

fn valid_percent_escapes(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

#[cfg(test)]
fn has_attachment(mail: &ParsedMessage<'_>) -> bool {
    !attachment_part_ids(mail).is_empty()
}

fn attachment_part_ids(mail: &ParsedMessage<'_>) -> Vec<u32> {
    let mut attachments = Vec::new();
    let mut pending = vec![0_u32];
    while let Some(part_id) = pending.pop() {
        let Some(part) = mail.part(part_id) else {
            continue;
        };
        if is_attachment_part(mail, part) {
            attachments.push(part_id);
            continue;
        }
        if let PartType::Multipart(children) = &part.body {
            pending.extend(children.iter().rev().copied());
        }
    }
    attachments
}

fn is_attachment_part(mail: &ParsedMessage<'_>, part: &MessagePart<'_>) -> bool {
    let content_type = mime_type(part);
    let is_text_body = matches!(content_type.as_str(), "text/plain" | "text/html");
    let is_message_attachment = matches!(
        content_type.as_str(),
        "message/rfc822" | "message/global" | "message/news"
    ) || part.is_message();
    let disposition = part.content_disposition();
    let has_nonempty_filename = supplied_attachment_name(mail, part).is_some();
    if part.is_multipart() {
        return has_nonempty_filename || disposition.is_some_and(|value| value.is_attachment());
    }
    let is_unnamed_text_body = is_text_body && !has_nonempty_filename;
    is_message_attachment
        // Non-text leaves are never message bodies. This also preserves an
        // explicitly empty `name` parameter that mail-parser intentionally
        // normalizes away from its typed Content-Type attributes.
        || part.is_binary()
        || (part.is_text() && !is_text_body)
        || disposition.is_some_and(|value| value.is_attachment())
        || if is_text_body {
            has_nonempty_filename
        } else {
            false
        }
        || (part.content_id().is_some() && !is_unnamed_text_body)
        || (disposition.is_some_and(|value| value.is_inline()) && !is_unnamed_text_body)
}

#[cfg(test)]
fn extract_attachments(mail: &ParsedMessage<'_>, message_id: &str) -> Result<Vec<AttachmentData>> {
    let selected_html = select_bodies(mail)?.html;
    extract_attachments_from_raw(
        mail,
        mail.raw_message(),
        message_id,
        selected_html.as_deref(),
    )
}

fn extract_attachments_from_raw(
    mail: &ParsedMessage<'_>,
    raw: &[u8],
    message_id: &str,
    selected_html: Option<&str>,
) -> Result<Vec<AttachmentData>> {
    let candidate_ids = attachment_part_ids(mail);
    if candidate_ids.len() > MAX_ATTACHMENT_COUNT {
        bail!("message has more than {MAX_ATTACHMENT_COUNT} attachments");
    }
    let mut attachments = Vec::new();
    let mut decoded_bytes = 0_usize;
    for part_id in candidate_ids {
        let Some(part) = mail.part(part_id) else {
            continue;
        };
        // An undecodable transfer encoding must never be stored as a plausible
        // attachment. Keeping it absent is safer than exposing raw transport bytes.
        if part.is_encoding_problem || !has_supported_transfer_encoding(part) {
            continue;
        }
        let Some(bytes) = decoded_part_bytes(raw, part) else {
            continue;
        };
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment exceeds the {} MiB safety limit",
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            );
        }
        add_decoded_attachment_bytes(&mut decoded_bytes, bytes.len())?;
        let presentation = attachment_presentation(mail, part, selected_html);
        if !presentation.is_downloadable() {
            continue;
        }
        let mime_type = mime_type(part);
        let supplied_name = supplied_attachment_name(mail, part).unwrap_or_else(|| {
            if mime_type.starts_with("message/") {
                "attached-message.eml".into()
            } else {
                "attachment".into()
            }
        });
        let mut filename = safe_attachment_filename(&supplied_name, attachments.len());
        if mime_type.starts_with("message/") && !filename.to_ascii_lowercase().ends_with(".eml") {
            filename.push_str(".eml");
        }
        let is_inline = part
            .content_disposition()
            .is_some_and(|value| value.is_inline())
            || part.content_id().is_some();
        let attachment = Attachment {
            id: format!("{message_id}:{}", attachments.len()),
            message_id: message_id.to_owned(),
            is_potentially_unsafe: is_potentially_unsafe(&filename, &mime_type),
            filename,
            mime_type,
            size_bytes: bytes.len() as i64,
            is_inline,
            presentation,
        };
        attachments.push(AttachmentData { attachment, bytes });
    }
    Ok(attachments)
}

fn add_decoded_attachment_bytes(total: &mut usize, part_bytes: usize) -> Result<()> {
    *total = total
        .checked_add(part_bytes)
        .context("attachment bytes overflowed the safety limit")?;
    if *total > MAX_ATTACHMENT_TOTAL_BYTES {
        bail!(
            "message attachments exceed the {} MiB safety limit",
            MAX_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
        );
    }
    Ok(())
}

fn attachment_presentation(
    mail: &ParsedMessage<'_>,
    part: &MessagePart<'_>,
    selected_html: Option<&str>,
) -> AttachmentPresentation {
    let explicitly_attached = part
        .content_disposition()
        .is_some_and(|disposition| disposition.is_attachment());
    let referenced = selected_html.is_some_and(|html| {
        attachment_references(part).iter().any(|reference| {
            html_contains_reference(html, reference)
                && attachment_reference_count(mail, reference) == 1
        })
    });
    match (explicitly_attached, referenced) {
        (true, true) => AttachmentPresentation::Both,
        (true, false) => AttachmentPresentation::Downloadable,
        (false, true) => AttachmentPresentation::Embedded,
        (false, false) => AttachmentPresentation::Downloadable,
    }
}

fn attachment_reference_count(mail: &ParsedMessage<'_>, reference: &str) -> usize {
    attachment_part_ids(mail)
        .into_iter()
        .filter_map(|part_id| mail.part(part_id))
        .filter(|part| {
            attachment_references(part)
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(reference))
        })
        .count()
}

fn attachment_references(part: &MessagePart<'_>) -> Vec<String> {
    let mut references = Vec::new();
    if let Some(content_id) = part.content_id() {
        let content_id = content_id.trim().trim_matches(['<', '>']);
        if !content_id.is_empty() {
            references.push(format!("cid:{content_id}"));
        }
    }
    if let Some(location) = part.content_location() {
        let location = location.trim();
        if !location.is_empty() {
            references.push(location.to_owned());
        }
    }
    references
}

fn html_contains_reference(html: &str, reference: &str) -> bool {
    html.to_ascii_lowercase()
        .contains(&reference.to_ascii_lowercase())
}

pub fn safe_attachment_filename(value: &str, index: usize) -> String {
    let value = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    ':' | '<' | '>' | '"' | '|' | '?' | '*'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect::<String>();
    let value = value.trim().trim_matches('.').trim();
    let value = if value.is_empty() {
        "attachment"
    } else {
        value
    };
    let truncated = truncate_filename_bytes(value, 180);
    if truncated.is_empty() {
        format!("attachment-{}", index + 1)
    } else {
        truncated
    }
}

fn truncate_filename_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let extension = value
        .rsplit_once('.')
        .map(|(_, extension)| format!(".{extension}"))
        .filter(|extension| extension.len() < max_bytes);
    let suffix = extension.as_deref().unwrap_or_default();
    let prefix_budget = max_bytes - suffix.len();
    let mut truncated = String::new();
    for character in value.chars() {
        if truncated.len() + character.len_utf8() > prefix_budget {
            break;
        }
        truncated.push(character);
    }
    truncated.push_str(suffix);
    truncated
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

    fn mime_corpus(name: &str) -> &'static [u8] {
        match name {
            "attached-message-rfc822" => {
                include_bytes!("../testdata/mime/attached-message-rfc822.eml")
            }
            "charset-matrix" => include_bytes!("../testdata/mime/charset-matrix.eml"),
            "competing-nested-alternatives" => {
                include_bytes!("../testdata/mime/competing-nested-alternatives.eml")
            }
            "disposition-type-name-matrix" => {
                include_bytes!("../testdata/mime/disposition-type-name-matrix.eml")
            }
            "filename-parameters-and-encodings" => {
                include_bytes!("../testdata/mime/filename-parameters-and-encodings.eml")
            }
            "format-flowed-delsp" => include_bytes!("../testdata/mime/format-flowed-delsp.eml"),
            "invalid-transfer-encodings" => {
                include_bytes!("../testdata/mime/invalid-transfer-encodings.eml")
            }
            "line-endings-cr-only" => include_bytes!("../testdata/mime/line-endings-cr-only.eml"),
            "linkedin-inline-content-id" => {
                include_bytes!("../testdata/mime/linkedin-inline-content-id.eml")
            }
            "multipart-attachment-container" => {
                include_bytes!("../testdata/mime/multipart-attachment-container.eml")
            }
            "truncated-multipart-lf" => {
                include_bytes!("../testdata/mime/truncated-multipart-lf.eml")
            }
            _ => panic!("unknown MIME corpus fixture: {name}"),
        }
    }

    #[tokio::test]
    async fn parses_the_checked_in_mime_regression_corpus() {
        let account = test_account();
        let linked_in = parse_message(
            &account,
            "INBOX",
            1,
            &[],
            mime_corpus("linkedin-inline-content-id"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            linked_in.body_text.trim(),
            "Redacted plain text=content with a foldedline."
        );
        assert!(linked_in
            .body_html
            .as_deref()
            .is_some_and(|body| body.contains("Redacted HTML=content")));
        assert_eq!(
            linked_in.snippet,
            "Redacted plain text=content with a foldedline."
        );
        assert!(linked_in.attachments.is_empty());

        let nested = parse_message(
            &account,
            "INBOX",
            2,
            &[],
            mime_corpus("competing-nested-alternatives"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(nested.body_text.trim(), "Visible plain body.");
        assert!(!nested.body_text.contains("redacted-notes"));
        assert!(nested
            .body_html
            .as_deref()
            .is_some_and(|body| body.contains("Visible <strong>HTML</strong> body.")));
        assert_eq!(nested.attachments.len(), 1);
        assert!(nested
            .attachments
            .iter()
            .any(|attachment| attachment.attachment.filename == "redacted-notes.txt"));

        let attached = parse_message(
            &account,
            "INBOX",
            3,
            &[],
            mime_corpus("attached-message-rfc822"),
            None,
        )
        .await
        .unwrap();
        assert!(attached
            .body_text
            .contains("The attached message is synthetic"));
        assert!(!attached.body_text.contains("Original plain text."));
        assert_eq!(attached.attachments.len(), 1);
        assert_eq!(
            attached.attachments[0].attachment.filename,
            "forwarded-message.eml"
        );
        assert_eq!(
            attached.attachments[0].attachment.mime_type,
            "message/rfc822"
        );
        let attached_bytes = std::str::from_utf8(&attached.attachments[0].bytes).unwrap();
        assert!(attached_bytes.starts_with("Date: Mon, 27 Jul 2026"));
        assert!(attached_bytes.contains("Original plain text."));
        assert!(!attached_bytes.contains("--outer"));

        let flowed = parse_message(
            &account,
            "INBOX",
            4,
            &[],
            mime_corpus("format-flowed-delsp"),
            None,
        )
        .await
        .unwrap();
        assert!(flowed.body_text.contains("soft spacethat continues"));
        assert!(flowed
            .body_text
            .contains("> Quoted flowed textcontinues too."));

        let filenames = parse_message(
            &account,
            "INBOX",
            5,
            &[],
            mime_corpus("filename-parameters-and-encodings"),
            None,
        )
        .await
        .unwrap();
        let filenames = filenames
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.filename.as_str())
            .collect::<Vec<_>>();
        assert!(filenames.contains(&"quarterly report final.pdf"));
        assert!(filenames.contains(&"report-test.txt"));
        assert!(filenames.contains(&"invoice;final.pdf"));
        assert!(filenames
            .iter()
            .all(|name| !name.contains(['/', '\\', '\u{202e}'])));

        let invalid = parse_message(
            &account,
            "INBOX",
            6,
            &[],
            mime_corpus("invalid-transfer-encodings"),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(invalid.to_string(), MIME_CONTENT_UNDECODABLE);

        let charset = parse_message(
            &account,
            "INBOX",
            7,
            &[],
            mime_corpus("charset-matrix"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(charset.body_text.trim(), "Zażółć");
        let parsed_charset = parse_complete_message(mime_corpus("charset-matrix")).unwrap();
        let charset_parts = parsed_charset
            .part(0)
            .and_then(MessagePart::sub_parts)
            .unwrap();
        let decoded = charset_parts
            .iter()
            .map(|part_id| {
                decode_text_candidate(&parsed_charset, parsed_charset.part(*part_id).unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(decoded[0].as_deref(), Ok("こんにちは"));
        assert_eq!(decoded[1].as_deref(), Ok("こんにちは"));
        assert_eq!(decoded[2].as_deref(), Ok("café"));
        assert_eq!(decoded[3].as_deref(), Ok("Zażółć"));
        assert!(decoded[4].is_err());

        let disposition = parse_message(
            &account,
            "INBOX",
            8,
            &[],
            mime_corpus("disposition-type-name-matrix"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(disposition.body_text.trim(), "Unnamed inline plain body.");
        assert!(disposition
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains("Unnamed inline HTML body.")));
        assert_eq!(disposition.attachments.len(), 4);
        assert!(disposition
            .attachments
            .iter()
            .any(|attachment| attachment.attachment.filename == "named-inline.txt"));

        let cr_only = parse_message(
            &account,
            "INBOX",
            9,
            &[],
            mime_corpus("line-endings-cr-only"),
            None,
        )
        .await
        .unwrap();
        assert!(cr_only.body_text.contains("CR-only plain text."));

        let multipart_attachment = parse_message(
            &account,
            "INBOX",
            10,
            &[],
            mime_corpus("multipart-attachment-container"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            multipart_attachment.body_text.trim(),
            "Visible parent body."
        );
        assert!(!multipart_attachment.body_text.contains("Secret child text"));
        assert_eq!(multipart_attachment.attachments.len(), 1);
        assert_eq!(
            multipart_attachment.attachments[0].attachment.filename,
            "bundle.mime"
        );
        assert_eq!(
            multipart_attachment.attachments[0].attachment.mime_type,
            "multipart/mixed"
        );
        assert!(
            std::str::from_utf8(&multipart_attachment.attachments[0].bytes)
                .unwrap()
                .contains("Secret child text must stay inside the attachment.")
        );

        assert_eq!(
            snippet_from_partial(mime_corpus("truncated-multipart-lf")),
            "The first leaf is complete and is a candidate for a safe partial preview."
        );
    }

    #[tokio::test]
    async fn every_mime_corpus_fixture_uses_all_publication_parse_paths() {
        let account = test_account();
        let cases = [
            ("attached-message-rfc822", true),
            ("charset-matrix", true),
            ("competing-nested-alternatives", true),
            ("disposition-type-name-matrix", true),
            ("filename-parameters-and-encodings", true),
            ("format-flowed-delsp", true),
            ("invalid-transfer-encodings", false),
            ("line-endings-cr-only", true),
            ("linkedin-inline-content-id", true),
            ("multipart-attachment-container", true),
            ("truncated-multipart-lf", true),
        ];

        for (index, (name, complete_succeeds)) in cases.into_iter().enumerate() {
            let raw = mime_corpus(name);
            let uid = index as u32 + 100;
            let complete = parse_message(&account, "INBOX", uid, &[], raw, None).await;
            if complete_succeeds {
                let complete = complete
                    .unwrap_or_else(|error| panic!("{name} complete parse failed: {error}"));
                assert!(!complete.subject.is_empty(), "{name}");
                assert_eq!(complete.content_state, "complete", "{name}");
            } else {
                assert_eq!(
                    complete.unwrap_err().to_string(),
                    MIME_CONTENT_UNDECODABLE,
                    "{name}"
                );
            }

            let catalog =
                parse_catalog_message(&account, "INBOX", uid, &[], raw, "catalog preview".into())
                    .unwrap_or_else(|error| panic!("{name} catalog parse failed: {error}"));
            let headers = parse_header_message(&account, "INBOX", uid, &[], raw)
                .unwrap_or_else(|error| panic!("{name} header-only parse failed: {error}"));
            assert_eq!(catalog.subject, headers.subject, "{name}");
            assert_eq!(catalog.from_address, headers.from_address, "{name}");
            assert_eq!(catalog.to_addresses, headers.to_addresses, "{name}");
            assert_eq!(catalog.received_at, headers.received_at, "{name}");
            assert_eq!(catalog.snippet, "catalog preview", "{name}");
            assert!(headers.snippet.is_empty(), "{name}");
            assert!(headers.body_text.is_empty(), "{name}");
            assert!(headers.attachments.is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn parsed_corpus_content_and_attachment_metadata_round_trip_through_storage() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("mail.sqlite");
        let store = Store::open(&database).await.unwrap();
        let account = test_account();
        store.save_account(&account).await.unwrap();
        let response_lines = vec![r#"* 1 FETCH (FLAGS (\Flagged) UID 42)"#.to_owned()];
        let message = parse_message(
            &account,
            "INBOX",
            42,
            &response_lines,
            mime_corpus("attached-message-rfc822"),
            None,
        )
        .await
        .unwrap();
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        let stored = reopened.message(&message.id).await.unwrap().unwrap();
        assert_eq!(stored.body_text, message.body_text);
        assert_eq!(stored.body_html, message.body_html);
        assert_eq!(stored.snippet, message.snippet);
        assert!(!stored.body_text.contains("Original plain text."));
        let metadata = reopened
            .starred_attachment_metadata(&message.id)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].filename, "forwarded-message.eml");
        assert_eq!(
            metadata[0].mime_type,
            message.attachments[0].attachment.mime_type
        );
        assert_eq!(
            metadata[0].size_bytes,
            message.attachments[0].attachment.size_bytes
        );
    }

    #[tokio::test]
    async fn duplicate_singleton_headers_use_the_first_value_in_every_parse_path() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "Date: Wed, 22 Jul 2026 11:00:00 +0000\r\n",
            "From: First Sender <first@example.test>\r\n",
            "From: Second Sender <second@example.test>\r\n",
            "To: recipient@example.test\r\n",
            "Subject: First subject\r\n",
            "Subject: Second subject\r\n",
            "\r\n",
            "First body"
        );

        let complete = parse_message(&account, "INBOX", 1, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        let catalog =
            parse_catalog_message(&account, "INBOX", 2, &[], raw.as_bytes(), String::new())
                .unwrap();
        let headers = parse_header_message(&account, "INBOX", 3, &[], raw.as_bytes()).unwrap();

        for message in [complete, catalog, headers] {
            assert_eq!(message.subject, "First subject");
            assert_eq!(message.from_name.as_deref(), Some("First Sender"));
            assert_eq!(message.from_address, "first@example.test");
            assert_eq!(
                message.received_at,
                chrono::DateTime::parse_from_rfc2822("Tue, 21 Jul 2026 10:00:00 +0000")
                    .unwrap()
                    .with_timezone(&Utc)
            );
        }
    }

    #[tokio::test]
    async fn related_start_selects_the_declared_root_and_only_resolves_references() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\n",
            "To: recipient@example.test\r\n",
            "Subject: Related start\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/related; boundary=rel; start=\"<chosen>\"\r\n",
            "\r\n",
            "--rel\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-ID: <not-chosen>\r\n\r\n",
            "<p>Wrong root</p>\r\n",
            "--rel\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-ID: <chosen>\r\n\r\n",
            "<p>Chosen root<img src=\"cid:used\"></p>\r\n",
            "--rel\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <used>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "iVBORw0KGgo=\r\n",
            "--rel\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <unused>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "iVBORw0KGgo=\r\n",
            "--rel--\r\n"
        );

        let message = parse_message(&account, "INBOX", 4, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        let html = message.body_html.unwrap();
        assert!(html.contains("Chosen root"));
        assert!(!html.contains("Wrong root"));
        assert!(html.contains("data:image/png;base64,"));
        assert!(!html.contains("unused"));
    }

    #[tokio::test]
    async fn content_location_rewriting_does_not_modify_html_text() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\n",
            "To: recipient@example.test\r\n",
            "Subject: Content location\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/related; boundary=rel\r\n\r\n",
            "--rel\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>a cat remains readable</p><img src=\"a\">\r\n",
            "--rel\r\nContent-Type: image/png\r\n",
            "Content-Location: a\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "iVBORw0KGgo=\r\n--rel--\r\n"
        );
        let message = parse_message(&account, "INBOX", 8, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        let html = message.body_html.unwrap();
        assert!(html.contains("a cat remains readable"));
        assert!(html.contains("src=\"data:image/png;base64,"));
    }

    #[tokio::test]
    async fn cr_only_normalization_preserves_unencoded_attachment_bytes() {
        let account = test_account();
        let mut raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r",
            "From: sender@example.test\r",
            "To: recipient@example.test\r",
            "Subject: CR-only binary\r",
            "MIME-Version: 1.0\r",
            "Content-Type: multipart/mixed; boundary=b\r\r",
            "--b\rContent-Type: text/plain; charset=utf-8\r\rBody\r",
            "--b\rContent-Type: application/octet-stream; name=data.bin\r",
            "Content-Disposition: attachment; filename=data.bin\r\r"
        )
        .as_bytes()
        .to_vec();
        raw.extend_from_slice(b"A\rB");
        raw.extend_from_slice(b"\r--b--\r");

        let message = parse_message(&account, "INBOX", 9, &[], &raw, None)
            .await
            .unwrap();
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.attachments[0].bytes, [b'A', b'\r', b'B']);
    }

    #[tokio::test]
    async fn repeated_cid_references_cannot_expand_reader_html_past_the_message_budget() {
        let account = test_account();
        let references = "<img src=\"cid:large\">".repeat(100);
        let image = STANDARD.encode(vec![0_u8; 600_000]);
        let raw = format!(
            concat!(
                "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
                "From: sender@example.test\r\n",
                "To: recipient@example.test\r\n",
                "Subject: CID budget\r\n",
                "MIME-Version: 1.0\r\n",
                "Content-Type: multipart/related; boundary=rel\r\n\r\n",
                "--rel\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
                "{}\r\n",
                "--rel\r\nContent-Type: image/png\r\n",
                "Content-ID: <large>\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n",
                "{}\r\n--rel--\r\n"
            ),
            references, image
        );
        let error = parse_message(&account, "INBOX", 10, &[], raw.as_bytes(), None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "mime_resolved_html_too_large");
    }

    #[tokio::test]
    async fn attachment_filename_precedence_ignores_empty_and_malformed_candidates() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\n",
            "To: recipient@example.test\r\n",
            "Subject: Filename precedence\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=files\r\n\r\n",
            "--files\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nBody\r\n",
            "--files\r\n",
            "Content-Type: application/octet-stream; name=\"type-name.txt\"\r\n",
            "Content-Disposition: attachment; filename=\"plain.txt\"; ",
            "filename*=utf-8''extended%2Etxt\r\n\r\none\r\n",
            "--files\r\n",
            "Content-Type: text/plain; charset=utf-8; name=\"notes.txt\"\r\n",
            "Content-Disposition: inline; filename=\"\"\r\n\r\nnotes\r\n",
            "--files\r\n",
            "Content-Type: application/octet-stream; name=\"fallback.bin\"\r\n",
            "Content-Disposition: attachment; filename*=utf-8''bad%ZZ.exe\r\n\r\nthree\r\n",
            "--files--\r\n"
        );

        let message = parse_message(&account, "INBOX", 5, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        let filenames = message
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.filename.as_str())
            .collect::<Vec<_>>();
        assert_eq!(filenames, vec!["extended.txt", "notes.txt", "fallback.bin"]);
        assert!(!message.body_text.contains("notes"));
    }

    #[tokio::test]
    async fn unknown_charset_requires_valid_utf8_when_it_is_the_only_body() {
        let account = test_account();
        let valid = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\n",
            "To: recipient@example.test\r\n",
            "Subject: Unknown charset\r\n",
            "Content-Type: text/plain; charset=x-unknown\r\n\r\n",
            "Valid UTF-8 café"
        );
        let valid = parse_message(&account, "INBOX", 6, &[], valid.as_bytes(), None)
            .await
            .unwrap();
        assert_eq!(valid.body_text, "Valid UTF-8 café");

        let mut invalid = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\n",
            "To: recipient@example.test\r\n",
            "Subject: Unknown charset\r\n",
            "Content-Type: text/plain; charset=x-unknown\r\n\r\n"
        )
        .as_bytes()
        .to_vec();
        invalid.extend_from_slice(&[0xff, 0xfe]);
        let error = parse_message(&account, "INBOX", 7, &[], &invalid, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), MIME_CONTENT_UNDECODABLE);
    }

    #[test]
    fn production_parser_enforces_raw_header_part_and_depth_budgets() {
        let oversized_raw = vec![b'x'; MAX_RAW_MESSAGE_BYTES + 1];
        assert_eq!(
            parse_complete_message(&oversized_raw)
                .unwrap_err()
                .to_string(),
            "mime_raw_message_too_large"
        );

        let oversized_headers = vec![b'x'; crate::mime_budget::MAX_MIME_HEADER_BYTES + 1];
        assert_eq!(
            parse_complete_message(&oversized_headers)
                .unwrap_err()
                .to_string(),
            "mime_headers_too_large"
        );

        let at_part_limit = multipart_with_leaf_count(999);
        parse_complete_message(&at_part_limit).unwrap();
        let over_part_limit = multipart_with_leaf_count(1_000);
        assert_eq!(
            parse_complete_message(&over_part_limit)
                .unwrap_err()
                .to_string(),
            "mime_too_many_parts"
        );

        let at_depth_limit = nested_multipart(64);
        parse_complete_message(&at_depth_limit).unwrap();
        let over_depth_limit = nested_multipart(65);
        assert_eq!(
            parse_complete_message(&over_depth_limit)
                .unwrap_err()
                .to_string(),
            "mime_multipart_nesting_too_deep"
        );

        let mut aggregate_headers = b"Content-Type: multipart/mixed; boundary=h\r\n\r\n".to_vec();
        for index in 0..2 {
            aggregate_headers.extend_from_slice(b"--h\r\nX-Fill: ");
            aggregate_headers.extend(std::iter::repeat_n(b'x', 525_000));
            aggregate_headers.extend_from_slice(
                format!(
                    "\r\nContent-Type: application/octet-stream; name={index}.bin\r\n\r\nx\r\n"
                )
                .as_bytes(),
            );
        }
        aggregate_headers.extend_from_slice(b"--h--\r\n");
        assert_eq!(
            parse_complete_message(&aggregate_headers)
                .unwrap_err()
                .to_string(),
            "mime_headers_too_large"
        );
    }

    fn multipart_with_leaf_count(count: usize) -> Vec<u8> {
        let mut raw = b"Content-Type: multipart/mixed; boundary=p\r\n\r\n".to_vec();
        for index in 0..count {
            raw.extend_from_slice(
                format!(
                    "--p\r\nContent-Type: application/octet-stream; name={index}.bin\r\n\r\nx\r\n"
                )
                .as_bytes(),
            );
        }
        raw.extend_from_slice(b"--p--\r\n");
        raw
    }

    fn nested_multipart(depth: usize) -> Vec<u8> {
        let mut raw = Vec::new();
        for level in 0..depth {
            raw.extend_from_slice(
                format!("Content-Type: multipart/mixed; boundary=b{level}\r\n\r\n--b{level}\r\n")
                    .as_bytes(),
            );
        }
        raw.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n");
        for level in (0..depth).rev() {
            raw.extend_from_slice(format!("--b{level}--\r\n").as_bytes());
        }
        raw
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

    #[tokio::test]
    async fn linkedin_content_id_alternatives_are_message_bodies() {
        let account = test_account();
        let raw = concat!(
            "Date: Wed, 29 Jul 2026 20:31:36 +0000\r\n",
            "From: Example Person via LinkedIn <messaging-digest-noreply@linkedin.com>\r\n",
            "To: Reader <reader@example.test>\r\n",
            "Subject: Example Person just messaged you\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=linkedin-body\r\n",
            "\r\n",
            "--linkedin-body\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Disposition: inline; filename=\"\"\r\n",
            "Content-ID: <linkedin-plain-body>\r\n",
            "\r\n",
            "You have a new message from Example Person.\r\n",
            "--linkedin-body\r\n",
            "Content-Type: text/html; charset=utf-8; name=\"\"\r\n",
            "Content-Disposition: inline\r\n",
            "Content-ID: <linkedin-html-body>\r\n",
            "\r\n",
            "<p>You have a new message from <strong>Example Person</strong>.</p>\r\n",
            "--linkedin-body--\r\n"
        );

        let message = parse_message(&account, "INBOX", 1, &[], raw.as_bytes(), None)
            .await
            .unwrap();

        assert_eq!(
            message.body_text.trim(),
            "You have a new message from Example Person."
        );
        assert!(message
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains("<strong>Example Person</strong>")));
        assert_eq!(
            message.snippet,
            "You have a new message from Example Person."
        );
        assert!(!message.has_attachments);
        assert!(message.attachments.is_empty());
    }

    #[test]
    fn catalogue_does_not_turn_a_named_inline_signature_image_into_a_paperclip() {
        let raw = b"Date: Tue, 21 Jul 2026 10:00:00 +0000\r\nSubject: Signature\r\n\r\n";
        let inline = parse_catalog_message(
            &test_account(),
            "INBOX",
            1,
            &["* 1 FETCH (BODYSTRUCTURE (\"IMAGE\" \"PNG\" NIL \"image001.png\" \"INLINE\" (\"FILENAME\" \"image001.png\")))".into()],
            raw,
            String::new(),
        )
        .unwrap();
        let attachment = parse_catalog_message(
            &test_account(),
            "INBOX",
            2,
            &["* 2 FETCH (BODYSTRUCTURE (\"APPLICATION\" \"PDF\" NIL \"claim.pdf\" \"ATTACHMENT\" (\"FILENAME\" \"claim.pdf\")))".into()],
            raw,
            String::new(),
        )
        .unwrap();

        assert!(!inline.has_attachments);
        assert!(attachment.has_attachments);
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
    fn raw_export_fetch_is_read_neutral_and_requests_only_the_original_bytes() {
        let command = raw_message_fetch_command(42);

        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[])");
        assert!(!command.contains("RFC822"));
        assert!(!command.contains("FLAGS"));
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
        let parsed = parse_complete_message(
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
            let parsed = parse_complete_message(raw).unwrap();
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
        let parsed = parse_complete_message(b"Subject: no date\r\n\r\nHi").unwrap();
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
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let html = extract_html(&parsed).unwrap();
        let resolved = resolve_inline_images(html, &parsed).unwrap();
        assert!(resolved.contains("<strong>Hello</strong>"));
        assert!(resolved.contains("src=\"data:image/png;base64,iVBORw0KGgo=\""));
    }

    #[tokio::test]
    async fn redacted_provider_signature_logo_stays_embedded_while_pdf_is_downloadable() {
        // This fictional fixture preserves the multipart/mixed ->
        // multipart/related shape, signature table, whitespace spacers, CID,
        // and Content-Location headers. The local cache did not contain raw
        // RFC822 source, so no production names, addresses, or content remain.
        let raw = include_str!("../tests/fixtures/provider-signature-inline.eml");
        let message = parse_message(&test_account(), "INBOX", 2965, &[], raw.as_bytes(), None)
            .await
            .unwrap();

        assert!(message
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains("data:image/png;base64,iVBORw0KGgo=")));
        assert!(message.has_attachments);
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.attachments[0].attachment.filename,
            "claim-documents.pdf"
        );
        assert_eq!(
            message.attachments[0].attachment.presentation,
            AttachmentPresentation::Downloadable
        );
    }

    #[tokio::test]
    async fn referenced_signature_logo_does_not_set_the_user_facing_attachment_flag() {
        let raw = concat!(
            "Date: Wed, 29 Jul 2026 14:34:00 +0000\r\n",
            "Content-Type: multipart/related; boundary=related\r\n\r\n",
            "--related\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<table><tr><td>Signature<img src=\"cid:image001.png@redacted\"></td></tr></table>\r\n",
            "--related\r\nContent-Type: image/png; name=image001.png\r\n",
            "Content-ID: <image001.png@redacted>\r\n",
            "Content-Location: image001.png\r\n",
            "Content-Disposition: inline; filename=image001.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n",
            "--related--\r\n"
        );
        let message = parse_message(&test_account(), "INBOX", 1, &[], raw.as_bytes(), None)
            .await
            .unwrap();

        assert!(message
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains("data:image/png;base64,iVBORw0KGgo=")));
        assert!(!message.has_attachments);
        assert!(message.attachments.is_empty());
    }

    #[test]
    fn classifies_explicit_cid_attachments_as_both_and_unreferenced_inline_files_as_downloadable() {
        let raw = concat!(
            "Content-Type: multipart/related; boundary=related\r\n\r\n",
            "--related\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<img src=\"CiD:invoice@example\">\r\n",
            "--related\r\nContent-Type: application/pdf; name=invoice.pdf\r\n",
            "Content-ID: <invoice@example>\r\n",
            "Content-Disposition: attachment; filename=invoice.pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\ncGRm\r\n",
            "--related\r\nContent-Type: image/png; name=unused.png\r\n",
            "Content-ID: <unused@example>\r\n",
            "Content-Disposition: inline; filename=unused.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naW1n\r\n",
            "--related--\r\n"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let attachments = extract_attachments(&parsed, "message-1").unwrap();

        assert_eq!(attachments.len(), 2);
        assert_eq!(
            attachments[0].attachment.presentation,
            AttachmentPresentation::Both
        );
        assert!(attachments[0].attachment.is_inline);
        assert_eq!(
            attachments[1].attachment.presentation,
            AttachmentPresentation::Downloadable
        );
        assert!(attachments[1].attachment.is_inline);
    }

    #[test]
    fn only_references_in_the_selected_html_branch_embed_inline_resources() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=mixed\r\n\r\n",
            "--mixed\r\nContent-Type: multipart/alternative; boundary=alternative\r\n\r\n",
            "--alternative\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<img src=\"cid:unselected@example\">\r\n",
            "--alternative\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<img src=\"cid:selected@example\">\r\n",
            "--alternative--\r\n",
            "--mixed\r\nContent-Type: image/png; name=selected.png\r\n",
            "Content-ID: <selected@example>\r\n",
            "Content-Disposition: inline; filename=selected.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naW1n\r\n",
            "--mixed\r\nContent-Type: image/png; name=unselected.png\r\n",
            "Content-ID: <unselected@example>\r\n",
            "Content-Disposition: inline; filename=unselected.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naW1n\r\n",
            "--mixed--\r\n"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let attachments = extract_attachments(&parsed, "message-1").unwrap();

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "unselected.png");
        assert_eq!(
            attachments[0].attachment.presentation,
            AttachmentPresentation::Downloadable
        );
    }

    fn related_inline_images(count: usize, referenced: bool) -> String {
        let mut html = String::from("<html><body>");
        if referenced {
            for index in 0..count {
                html.push_str(&format!("<img src=\"cid:image-{index}@example.test\">"));
            }
        }
        html.push_str("</body></html>");
        let mut raw = format!(
            "Content-Type: multipart/related; boundary=related\r\n\r\n--related\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n"
        );
        for index in 0..count {
            raw.push_str(&format!(
                "--related\r\nContent-Type: image/png; name=image-{index}.png\r\nContent-ID: <image-{index}@example.test>\r\nContent-Disposition: inline; filename=image-{index}.png\r\nContent-Transfer-Encoding: base64\r\n\r\naW1n\r\n"
            ));
        }
        raw.push_str("--related--\r\n");
        raw
    }

    #[test]
    fn bounds_embedded_and_mixed_attachment_candidates_before_filtering_them() {
        let fifty = related_inline_images(MAX_ATTACHMENT_COUNT, true);
        let parsed = parse_complete_message(fifty.as_bytes()).unwrap();
        assert!(extract_attachments(&parsed, "message-1")
            .unwrap()
            .is_empty());

        let fifty_one = related_inline_images(MAX_ATTACHMENT_COUNT + 1, true);
        let parsed = parse_complete_message(fifty_one.as_bytes()).unwrap();
        assert!(extract_attachments(&parsed, "message-1")
            .unwrap_err()
            .to_string()
            .contains("more than 50 attachments"));

        let mut mixed = related_inline_images(MAX_ATTACHMENT_COUNT - 1, true);
        mixed = mixed.replacen(
            "--related--\r\n",
            "--related\r\nContent-Type: application/pdf; name=first.pdf\r\nContent-Disposition: attachment; filename=first.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\ncGRm\r\n--related\r\nContent-Type: application/pdf; name=second.pdf\r\nContent-Disposition: attachment; filename=second.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\ncGRm\r\n--related--\r\n",
            1,
        );
        let parsed = parse_complete_message(mixed.as_bytes()).unwrap();
        assert!(extract_attachments(&parsed, "message-1").is_err());
    }

    #[test]
    fn bounds_total_decoded_bytes_even_for_embedded_only_resources() {
        let mut decoded_bytes = MAX_ATTACHMENT_TOTAL_BYTES;
        assert!(add_decoded_attachment_bytes(&mut decoded_bytes, 1)
            .unwrap_err()
            .to_string()
            .contains("attachments exceed"));
    }

    #[test]
    fn duplicate_or_missing_cid_targets_remain_downloadable_instead_of_being_guessed_embedded() {
        let duplicate = concat!(
            "Content-Type: multipart/related; boundary=related\r\n\r\n",
            "--related\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<img src=\"cid:logo@example\">\r\n",
            "--related\r\nContent-Type: image/png; name=one.png\r\nContent-ID: <logo@example>\r\nContent-Disposition: inline; filename=one.png\r\nContent-Transfer-Encoding: base64\r\n\r\naW1n\r\n",
            "--related\r\nContent-Type: image/png; name=two.png\r\nContent-ID: <logo@example>\r\nContent-Disposition: inline; filename=two.png\r\nContent-Transfer-Encoding: base64\r\n\r\naW1n\r\n--related--\r\n"
        );
        let parsed = parse_complete_message(duplicate.as_bytes()).unwrap();
        assert_eq!(extract_attachments(&parsed, "message-1").unwrap().len(), 2);

        let missing = related_inline_images(1, false)
            .replace("</body>", "<img src=\"cid:missing@example.test\"></body>");
        let parsed = parse_complete_message(missing.as_bytes()).unwrap();
        assert_eq!(extract_attachments(&parsed, "message-1").unwrap().len(), 1);
    }

    #[test]
    fn attached_message_branches_cannot_supply_html_or_inline_resources_to_the_outer_message() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<img src=\"cid:outer@example\">\r\n",
            "--outer\r\nContent-Type: image/png\r\nContent-ID: <outer@example>\r\nContent-Disposition: inline; filename=outer.png\r\nContent-Transfer-Encoding: base64\r\n\r\nb3V0ZXI=\r\n",
            "--outer\r\nContent-Type: message/rfc822; name=forwarded.eml\r\nContent-Disposition: attachment; filename=forwarded.eml\r\n\r\n",
            "Content-Type: multipart/related; boundary=inner\r\n\r\n",
            "--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<img src=\"cid:outer@example\">\r\n",
            "--inner\r\nContent-Type: image/png\r\nContent-ID: <outer@example>\r\nContent-Disposition: inline; filename=inner.png\r\nContent-Transfer-Encoding: base64\r\n\r\naW5uZXI=\r\n",
            "--inner--\r\n--outer--\r\n"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let html = extract_html(&parsed).unwrap();
        let resolved = resolve_inline_images(html, &parsed).unwrap();
        let attachments = extract_attachments(&parsed, "message-1").unwrap();

        assert!(resolved.contains("b3V0ZXI="));
        assert!(!resolved.contains("aW5uZXI="));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "forwarded.eml");
    }

    #[test]
    fn html_only_messages_get_searchable_plain_text() {
        let raw = b"Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>Tallinn</b></p>";
        let parsed = parse_complete_message(raw).unwrap();
        let html = extract_html(&parsed).unwrap();
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
            "Content-Disposition: inline; filename=\"\"\r\n",
            "Content-ID: <linkedin-plain-body>\r\n",
            "\r\n",
            "Plain sent body\r\n",
            "--body\r\n",
            "Content-Type: text/html; charset=utf-8; name=\"\"\r\n",
            "Content-Disposition: inline\r\n",
            "Content-ID: <linkedin-html-body>\r\n",
            "\r\n",
            "<p>HTML sent body</p>\r\n",
            "--body--\r\n"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();

        assert_eq!(extract_text(&parsed).trim(), "Plain sent body");
        assert_eq!(
            extract_html(&parsed).unwrap().trim(),
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
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();

        assert!(extract_text(&parsed).is_empty());
        let attachments = extract_attachments(&parsed, "message-1").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "notes.txt");
    }

    #[test]
    fn empty_non_text_name_parameter_remains_an_attachment() {
        let raw = concat!(
            "Content-Type: application/pdf; name=\"\"\r\n",
            "\r\n",
            "%PDF"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let attachments = extract_attachments(&parsed, "message-1").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment.filename, "attachment");
        assert_eq!(attachments[0].attachment.mime_type, "application/pdf");
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
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
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
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
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
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
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
        let long_name = format!("{}.exe", "🧪".repeat(100));
        let sanitized = safe_attachment_filename(&long_name, 0);
        assert!(sanitized.len() <= 180);
        assert!(sanitized.ends_with(".exe"));
        assert!(is_potentially_unsafe(
            &sanitized,
            "application/octet-stream"
        ));
    }

    #[test]
    fn extracts_safe_structural_classification_signals() {
        let raw = b"List-Unsubscribe: <https://example.test/unsubscribe?recipient=private>\r\nList-Id: <product.example.test>\r\nPrecedence: bulk\r\nAuto-Submitted: auto-generated\r\nReply-To: support@example.test\r\n\r\nHello";
        let parsed = parse_complete_message(raw).unwrap();
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
        let parsed = parse_complete_message(b"Subject: hello\r\n\r\nHi").unwrap();
        let signals = classification_signals(
            &parsed,
            &["* 1 FETCH (X-GM-LABELS (\\Inbox \\Category_Promotions))".into()],
        );

        assert!(signals.contains("Gmail category: Promotions"));
        assert!(!signals.contains("\\Inbox"));
    }
}
