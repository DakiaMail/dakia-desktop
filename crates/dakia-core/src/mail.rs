#[cfg(test)]
use crate::storage::ThreadingHeaders;
use crate::{
    flowed::decode_format_flowed,
    mime_budget::{
        preflight_raw_message, validate_header_bytes, validate_structure, MAX_MIME_HEADER_BYTES,
        MAX_MIME_PARTS, MAX_MULTIPART_NESTING, MAX_RAW_MESSAGE_BYTES,
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
    transport::smtp::{
        authentication::{Credentials, Mechanism, DEFAULT_MECHANISMS},
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::{ClientId, Extension, MailBodyParameter, MailParameter},
    },
    Address, Message,
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
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    ops::Range,
    sync::Arc,
};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::TcpStream,
    sync::watch,
    time::{timeout, timeout_at, Duration, Instant},
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
/// Display hydration is deliberately sectioned.  These limits apply before a
/// server literal is allocated, so a malformed BODYSTRUCTURE cannot turn an
/// ordinary message open into an unbounded allocation.
const MAX_DISPLAY_PART_BYTES: usize = 25 * 1024 * 1024;
const MAX_DISPLAY_TOTAL_BYTES: usize = 50 * 1024 * 1024;
/// Base64 transport and line folding can make a valid 25 MiB attachment
/// appreciably larger on the wire.  The decoded limit remains authoritative.
const MAX_TARGETED_ATTACHMENT_ENCODED_BYTES: usize = 36 * 1024 * 1024;
/// Do not let a malicious BODYSTRUCTURE response grow an unbounded String
/// before the MIME parser has a chance to enforce its structural limits.
const MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES: usize = MAX_MIME_HEADER_BYTES;
const MAX_IMAP_RESPONSE_LITERALS: usize = 64;
/// Explicit raw export/full-forward operations may need transport encoding
/// overhead beyond decoded attachment limits, but never receive an unbounded
/// server response.
const MAX_IMAP_RESPONSE_LITERAL_BYTES: usize = 100 * 1024 * 1024;
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
        client.authenticate(account, &secret).await?;
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
            let mut response = client
                .command_with_literal_limited(
                    &format!("UID FETCH {uid} ({fields})"),
                    MAX_MIME_HEADER_BYTES,
                )
                .await?;
            if let Some(raw) = response.take_header_literal_for(uid) {
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
        let message = self.fetch_message(account, mailbox, uid).await?;
        self.store
            .upsert_messages(std::slice::from_ref(&message))
            .await?;
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
        let mut client = ImapClient::connect(account).await?;
        self.sync_mailboxes_with_progress_on_client(
            &mut client,
            account,
            max_messages,
            plans,
            reset_before_sync,
            on_progress,
        )
        .await
    }

    /// The transport-independent sync core is shared by the TLS production
    /// path and scripted protocol tests. Keeping connection construction in
    /// `sync_mailboxes_with_progress` preserves the production TLS policy.
    async fn sync_mailboxes_with_progress_on_client<S, F>(
        &self,
        client: &mut ImapClient<S>,
        account: &Account,
        max_messages: u32,
        plans: Vec<MailboxPlan>,
        reset_before_sync: bool,
        mut on_progress: F,
    ) -> Result<SyncResult>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        F: FnMut(SyncProgress),
    {
        on_progress(SyncProgress {
            phase: "authenticating",
            completed: 0,
            total: None,
        });
        let secret = self.credentials.secret(account).await?;
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
                    let mut response = client
                        .command_with_literal_limited(
                            &format!("UID FETCH {uid} ({fields})"),
                            MAX_MIME_HEADER_BYTES,
                        )
                        .await?;
                    if let Some(headers) = response.take_header_literal_for(*uid) {
                        if !item.plan.skip_gmail_system_labels
                            || gmail_all_mail_is_archive(&response.lines)
                        {
                            let snippet = client
                                .command_with_literal(&format!(
                                    "UID FETCH {uid} (BODY.PEEK[]<0.8192>)"
                                ))
                                .await
                                .ok()
                                .and_then(|mut response| {
                                    response.take_body_literal_for(*uid, "TEXT")
                                })
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

    /// Refreshes a small, recent catalogue window for the primary reader
    /// folders without hydrating message bodies. `max_messages` is an account
    /// total (not a per-folder multiplier): work is allocated round-robin in
    /// INBOX, Sent, Archive order, while each mailbox contributes its newest
    /// UIDs first. This keeps a busy INBOX from starving Sent and Archive.
    ///
    /// The method deliberately requires established catalogue identities. A
    /// UIDVALIDITY change is reported to the caller for a full sync; this
    /// refresh never resets a mailbox or marks historical work complete.
    pub async fn refresh_recent_main_mailboxes(
        &self,
        account: &Account,
        cutoff: chrono::DateTime<Utc>,
        max_messages: u32,
    ) -> Result<Vec<MailSummary>> {
        if max_messages == 0 {
            return Ok(Vec::new());
        }
        let secret = self.credentials.secret(account).await?;
        let mut client = ImapClient::connect(account).await?;
        client.authenticate(account, &secret).await?;
        let listing = client.command("LIST \"\" \"*\"").await.unwrap_or_default();
        let plans =
            refresh_main_mailbox_plans(resolve_special_mailboxes(mailbox_plans(account), &listing));
        let since = imap_since_date(cutoff);
        let mut work = Vec::new();

        // First establish every selected mailbox identity and its recent UID
        // set. No catalogue writes occur until all UIDVALIDITY checks pass.
        for plan in plans {
            let selected = match client
                .command(&format!("SELECT {}", quote_imap(&plan.remote)))
                .await
            {
                Ok(selected) => selected,
                Err(error) if plan.local != "INBOX" => {
                    tracing::debug!(%error, mailbox = %plan.remote, "recent refresh mailbox unavailable");
                    continue;
                }
                Err(error) => return Err(error).context("IMAP server does not expose an inbox"),
            };
            let uid_validity = parse_uid_validity(&selected)
                .context("IMAP server omitted UIDVALIDITY after SELECT")?;
            let state = match self
                .store
                .mailbox_catalog_state(account.id, &plan.storage)
                .await?
            {
                Some(state) => state,
                None if plan.local != "INBOX" => continue,
                None => bail!(
                    "mailbox catalogue is not initialized; sync the account before refreshing recent mail"
                ),
            };
            verify_mailbox_uid_validity(uid_validity, state.uid_validity, "refreshing recent")?;
            let search = client.command(&recent_uid_search_command(&since)).await?;
            work.push(RecentRefreshWork {
                plan,
                state,
                uid_validity,
                uids: recent_uids_newest_first(parse_search_uids(&search)),
                selected_uids: Vec::new(),
            });
        }
        allocate_recent_refresh_uids(&mut work, max_messages as usize);

        let mut refreshed = Vec::new();
        for item in &work {
            if item.selected_uids.is_empty() {
                continue;
            }
            let expected_flags = self
                .store
                .capture_recent_catalogue_expected_flags(
                    account.id,
                    &item.plan.storage,
                    &item.selected_uids,
                )
                .await?;
            client
                .command(&format!("SELECT {}", quote_imap(&item.plan.remote)))
                .await?;
            let mut messages = Vec::with_capacity(item.selected_uids.len());
            for uid in &item.selected_uids {
                let mut response = client
                    .command_with_literal_limited(
                        &format!("UID FETCH {uid} ({})", catalogue_fetch_fields(account)),
                        MAX_DISPLAY_PART_BYTES,
                    )
                    .await?;
                if item.plan.skip_gmail_system_labels && !gmail_all_mail_is_archive(&response.lines)
                {
                    continue;
                }
                let snippet = recent_catalogue_snippet(&mut client, *uid, &response).await?;
                let Some(headers) = response.take_header_literal_for(*uid) else {
                    continue;
                };
                messages.push(parse_catalog_message(
                    account,
                    &item.plan.storage,
                    *uid,
                    &response.lines,
                    &headers,
                    snippet,
                )?);
            }
            // Inserts carry provider FLAGS. Existing rows accept them only
            // when their local flags still match the pre-FETCH snapshot, so
            // remote-client changes propagate without undoing a concurrent
            // local read/star action.
            // We intentionally do not call `reconcile_mailbox_uids`: a SINCE
            // result cannot distinguish an absent recent row from an older
            // local row, so full reconciliation here could delete history.
            self.store
                .upsert_recent_catalog_messages(&messages, &expected_flags)
                .await?;
            self.store
                .save_mailbox_catalog_state(
                    account.id,
                    &item.plan.storage,
                    &item.plan.remote,
                    item.uid_validity,
                    item.state.remote_total.max(0) as usize,
                    item.state.historical_complete,
                )
                .await?;
            refreshed.extend(messages);
        }
        let _ = client.command("LOGOUT").await;
        Ok(refreshed)
    }

    /// Fetches display-safe message content without retrieving the complete
    /// RFC822 source or ordinary attachment bodies.  `AttachmentData::bytes`
    /// is intentionally empty; callers that need bytes must use
    /// [`Self::fetch_attachment`] or [`Self::fetch_full_message`].
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
        verify_mailbox_uid_validity(current_uid_validity, state.uid_validity, "opening")?;
        let fields = selective_metadata_fetch_fields(account);
        let mut response = client
            .command_with_literal_limited(
                &format!("UID FETCH {uid} ({fields})"),
                MAX_MIME_HEADER_BYTES,
            )
            .await?;
        let structure = parse_bodystructure_response(&response)?;
        let headers = response
            .take_header_literal_for(uid)
            .context("message is no longer available")?;
        let message = fetch_selective_message(
            &mut client,
            account,
            mailbox,
            uid,
            &response.lines,
            &headers,
            &structure,
        )
        .await?;
        let _ = client.command("LOGOUT").await;
        Ok(message)
    }

    /// Fetches the complete RFC822 message for explicit operations such as
    /// forwarding or saving every attachment.  This remains read-neutral but
    /// is intentionally not used by reader display/prefetch.
    pub async fn fetch_full_message(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
    ) -> Result<MailSummary> {
        let (mut client, _state) = self
            .select_catalogued_mailbox(account, mailbox, "opening")
            .await?;
        let mut response = client
            .command_with_literal_limited(
                &hydration_fetch_command(account, uid),
                MAX_IMAP_RESPONSE_LITERAL_BYTES,
            )
            .await?;
        let raw = response
            .take_body_literal_for(uid, "")
            .context("message is no longer available")?;
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

    /// Fetches exactly one downloadable attachment identified by the metadata
    /// returned from [`Self::fetch_message`].
    pub async fn fetch_attachment(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        attachment_id: &str,
    ) -> Result<AttachmentData> {
        let (mut client, _state) = self
            .select_catalogued_mailbox(account, mailbox, "saving")
            .await?;
        let mut metadata = client
            .command_with_literal_limited(
                &format!(
                    "UID FETCH {uid} ({})",
                    selective_metadata_fetch_fields(account)
                ),
                MAX_MIME_HEADER_BYTES,
            )
            .await?;
        let structure = parse_bodystructure_response(&metadata)?;
        let headers = metadata
            .take_header_literal_for(uid)
            .context("message is no longer available")?;
        let plan = selective_plan(&structure)?;
        let id = stable_message_id(account.id, mailbox, uid);
        let attachment = plan
            .attachments
            .into_iter()
            .find(|part| part.attachment_id(&id) == attachment_id)
            .context("attachment is not available for download")?;
        let header =
            fetch_section_mime_headers(&mut client, uid, &attachment.part.path, Some(&headers))
                .await?;
        let mut response = client
            .command_with_literal_limited(
                &section_fetch_command(uid, &attachment.part.path),
                MAX_TARGETED_ATTACHMENT_ENCODED_BYTES,
            )
            .await?;
        let bytes = response
            .take_body_literal_for(uid, &response_body_section(&attachment.part.path))
            .context("attachment is no longer available")?;
        let data = attachment_data_from_part(&attachment, &id, &header, Some(bytes), false)?;
        validate_targeted_attachment_bytes(&data.bytes)?;
        let _ = client.command("LOGOUT").await;
        Ok(data)
    }

    async fn select_catalogued_mailbox(
        &self,
        account: &Account,
        mailbox: &str,
        action: &str,
    ) -> Result<(ImapClient, crate::storage::MailboxCatalogState)> {
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
        verify_mailbox_uid_validity(current_uid_validity, state.uid_validity, action)?;
        Ok((client, state))
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
        verify_mailbox_uid_validity(current_uid_validity, state.uid_validity, "exporting")?;
        let mut response = client
            .command_with_literal(&raw_message_fetch_command(uid))
            .await?;
        let raw = response
            .take_body_literal_for(uid, "")
            .context("message is no longer available")?;
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
                    let mut response = client
                        .command_with_literal_limited(
                            &format!("UID FETCH {uid} ({fields})"),
                            MAX_MIME_HEADER_BYTES,
                        )
                        .await?;
                    if let Some(headers) = response.take_header_literal_for(uid) {
                        if plan.skip_gmail_system_labels
                            && !gmail_all_mail_is_archive(&response.lines)
                        {
                            continue;
                        }
                        let snippet = client
                            .command_with_literal(&format!("UID FETCH {uid} (BODY.PEEK[]<0.8192>)"))
                            .await
                            .ok()
                            .and_then(|mut response| response.take_body_literal_for(uid, "TEXT"))
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
        let endpoint = SmtpEndpoint {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            tls_parameters: TlsParameters::new(account.smtp_host.clone())?,
        };
        self.send_with_smtp_endpoint(account, draft, &secret, endpoint, SMTP_SEND_TIMEOUT)
            .await
    }

    async fn send_with_smtp_endpoint(
        &self,
        account: &Account,
        draft: &ComposeMessage,
        secret: &str,
        endpoint: SmtpEndpoint,
        deadline: Duration,
    ) -> Result<String> {
        let email = build_compose_message(account, draft)?;
        let raw_email = email.formatted();
        let response = send_smtp_raw(
            account,
            email.envelope(),
            &raw_email,
            secret,
            endpoint,
            deadline,
        )
        .await?;
        if !smtp_saves_sent_copy(account) {
            self.append_sent_copy(account, secret, &raw_email)
                .await
                .context(
                    "message was sent, but it could not be saved in the account's Sent folder",
                )?;
        }
        Ok(response)
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

async fn send_smtp_raw(
    account: &Account,
    envelope: &Envelope,
    raw_email: &[u8],
    secret: &str,
    endpoint: SmtpEndpoint,
    deadline: Duration,
) -> Result<String> {
    let deadline_at = Instant::now() + deadline;
    let client_id = ClientId::default();
    let implicit_tls =
        matches!(account.smtp_security, Security::Tls).then(|| endpoint.tls_parameters.clone());
    let mut connection = smtp_before_data(
        deadline_at,
        AsyncSmtpConnection::connect_tokio1(
            (&*endpoint.host, endpoint.port),
            None,
            &client_id,
            implicit_tls,
            None,
        ),
    )
    .await?;

    if matches!(account.smtp_security, Security::StartTls) {
        smtp_before_data(
            deadline_at,
            connection.starttls(endpoint.tls_parameters, &client_id),
        )
        .await?;
    }

    let credentials = Credentials::new(account.auth.username().to_owned(), secret.to_owned());
    let mechanisms: &[Mechanism] = match account.auth {
        AccountAuth::OAuth2 { .. } => &[Mechanism::Xoauth2],
        AccountAuth::Password { .. } => DEFAULT_MECHANISMS,
    };
    smtp_before_data(deadline_at, connection.auth(mechanisms, &credentials)).await?;

    let mut mail_options = Vec::new();
    let has_non_ascii_address = envelope
        .from()
        .into_iter()
        .chain(envelope.to())
        .any(|address| !address.to_string().is_ascii());
    if has_non_ascii_address {
        if !connection
            .server_info()
            .supports_feature(Extension::SmtpUtfEight)
        {
            bail!("Envelope contains non-ascii chars but server does not support SMTPUTF8");
        }
        mail_options.push(MailParameter::SmtpUtfEight);
    }
    if !raw_email.is_ascii() {
        if !connection
            .server_info()
            .supports_feature(Extension::EightBitMime)
        {
            bail!("Message contains non-ascii chars but server does not support 8BITMIME");
        }
        mail_options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }

    smtp_before_data(
        deadline_at,
        connection.command(Mail::new(envelope.from().cloned(), mail_options)),
    )
    .await?;
    for recipient in envelope.to() {
        smtp_before_data(
            deadline_at,
            connection.command(Rcpt::new(recipient.clone(), Vec::new())),
        )
        .await?;
    }
    smtp_before_data(deadline_at, connection.command(Data)).await?;

    // Once the DATA terminator has been written, a missing final response is
    // deliberately not retried as an ordinary failure: the relay may have
    // accepted and queued the message before the connection disappeared.
    let response = match timeout_at(deadline_at, connection.message(raw_email)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) if error.is_transient() || error.is_permanent() => return Err(error.into()),
        Ok(Err(error)) => {
            return Err(error).context(
                "SMTP final delivery status was not received after message submission; delivery may be uncertain",
            );
        }
        Err(_) => bail!(
            "SMTP final delivery status timed out after message submission; delivery may be uncertain"
        ),
    };
    Ok(response.message().collect::<Vec<_>>().join(" "))
}

/// The logical account and the connection endpoint are normally identical.
/// Tests may point the transport at a local TLS relay while retaining the
/// account's provider-specific Sent-copy policy.
struct SmtpEndpoint {
    host: String,
    port: u16,
    tls_parameters: TlsParameters,
}

async fn smtp_before_data<T>(
    deadline_at: Instant,
    operation: impl std::future::Future<Output = std::result::Result<T, lettre::transport::smtp::Error>>,
) -> Result<T> {
    Ok(timeout_at(deadline_at, operation)
        .await
        .context("SMTP send timed out before message submission")??)
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
struct ImapClient<S = TlsStream<TcpStream>> {
    reader: BufReader<S>,
    tag: u32,
}
struct ImapResponse {
    lines: Vec<String>,
    /// Every server literal in transcript order. The associated FETCH item
    /// keeps a literal BODYSTRUCTURE parameter from being mistaken for the
    /// header/body requested by a caller.
    literals: Vec<ImapLiteral>,
}

struct ImapLiteral {
    /// UID from the untagged FETCH that introduced this literal.  IMAP may
    /// interleave unsolicited FETCH replies while a command is outstanding.
    uid: Option<u32>,
    data_item: ImapLiteralItem,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImapLiteralItem {
    BodyStructure,
    Body(String),
    Other,
}

impl ImapResponse {
    fn take_header_literal_for(&mut self, uid: u32) -> Option<Vec<u8>> {
        self.take_literal_for(uid, |item| {
            matches!(item, ImapLiteralItem::Body(value) if body_item_section(value).is_some_and(|section| section.to_ascii_uppercase().starts_with("HEADER")))
        })
    }

    fn take_body_literal_for(&mut self, uid: u32, section: &str) -> Option<Vec<u8>> {
        self.take_literal_for(uid, |item| {
            matches!(item, ImapLiteralItem::Body(value) if body_item_section(value).is_some_and(|actual| actual.eq_ignore_ascii_case(section)))
        })
    }

    fn take_literal_for(
        &mut self,
        uid: u32,
        matches: impl Fn(&ImapLiteralItem) -> bool,
    ) -> Option<Vec<u8>> {
        self.literals
            .iter()
            .position(|literal| literal.uid == Some(uid) && matches(&literal.data_item))
            .map(|index| self.literals.remove(index).bytes)
    }
}

fn body_item_section(value: &str) -> Option<&str> {
    let start = value.find('[')? + 1;
    let end = value[start..].find(']')? + start;
    Some(&value[start..end])
}

#[derive(Debug)]
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

struct RecentRefreshWork {
    plan: MailboxPlan,
    state: crate::storage::MailboxCatalogState,
    uid_validity: u32,
    uids: Vec<u32>,
    selected_uids: Vec<u32>,
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

fn refresh_main_mailbox_plans(plans: Vec<MailboxPlan>) -> Vec<MailboxPlan> {
    plans
        .into_iter()
        .filter(|plan| matches!(plan.local, "INBOX" | "Sent" | "Archive"))
        .collect()
}

fn imap_since_date(cutoff: chrono::DateTime<Utc>) -> String {
    cutoff.format("%d-%b-%Y").to_string()
}

fn recent_uid_search_command(since: &str) -> String {
    format!("UID SEARCH SINCE {since}")
}

fn recent_uids_newest_first(mut uids: Vec<u32>) -> Vec<u32> {
    uids.sort_unstable_by(|left, right| right.cmp(left));
    uids.dedup();
    uids
}

fn allocate_recent_refresh_uids(work: &mut [RecentRefreshWork], max_messages: usize) {
    for item in work.iter_mut() {
        item.selected_uids.clear();
    }
    let mut remaining = max_messages;
    while remaining > 0 {
        let mut allocated = false;
        for item in work.iter_mut() {
            if remaining == 0 {
                break;
            }
            if let Some(uid) = item.uids.get(item.selected_uids.len()).copied() {
                item.selected_uids.push(uid);
                remaining -= 1;
                allocated = true;
            }
        }
        if !allocated {
            break;
        }
    }
}

fn catalogue_fetch_fields(account: &Account) -> &'static str {
    if account.provider_id == "gmail" {
        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE X-GM-LABELS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
    } else {
        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
    }
}

fn section_partial_fetch_command(uid: u32, path: &[usize], length: usize) -> String {
    let section = if path.is_empty() {
        "TEXT".into()
    } else {
        section_name(path)
    };
    format!("UID FETCH {uid} (BODY.PEEK[{section}]<0.{length}>)")
}

async fn recent_catalogue_snippet(
    client: &mut ImapClient,
    uid: u32,
    metadata_response: &ImapResponse,
) -> Result<String> {
    let structure = match parse_bodystructure_response(metadata_response)
        .and_then(|structure| selective_plan(&structure))
    {
        Ok(plan) => plan,
        Err(_) => return Ok(String::new()),
    };
    let Some(part) = structure.text_parts.first() else {
        return Ok(String::new());
    };
    let mut response = client
        .command_with_literal_limited(
            &section_partial_fetch_command(uid, &part.part.path, 8192),
            8192,
        )
        .await?;
    let section = if part.part.path.is_empty() {
        "TEXT".to_owned()
    } else {
        section_name(&part.part.path)
    };
    let Some(raw) = response.take_body_literal_for(uid, &section) else {
        return Ok(String::new());
    };
    Ok(snippet_from_partial(&mime_part_headers(&part.part, &raw)))
}

fn mime_part_headers(part: &MimePart, body: &[u8]) -> Vec<u8> {
    let mut headers = format!("Content-Type: {}", part.mime_type);
    for (name, value) in &part.params {
        headers.push_str(&format!("; {name}=\"{value}\""));
    }
    if !part.transfer_encoding.is_empty() {
        headers.push_str(&format!(
            "\r\nContent-Transfer-Encoding: {}",
            part.transfer_encoding
        ));
    }
    headers.push_str("\r\n\r\n");
    let mut result = headers.into_bytes();
    result.extend_from_slice(body);
    result
}

fn selective_metadata_fetch_fields(account: &Account) -> &'static str {
    if account.provider_id == "gmail" {
        "FLAGS INTERNALDATE X-GM-LABELS BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
    } else {
        "FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]"
    }
}

fn section_name(path: &[usize]) -> String {
    path.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn section_fetch_command(uid: u32, path: &[usize]) -> String {
    let section = response_body_section(path);
    format!("UID FETCH {uid} (BODY.PEEK[{section}])")
}

fn response_body_section(path: &[usize]) -> String {
    if path.is_empty() {
        "TEXT".into()
    } else {
        section_name(path)
    }
}

fn section_mime_fetch_command(uid: u32, path: &[usize]) -> String {
    let section = if path.is_empty() {
        "MIME".into()
    } else {
        format!("{}.MIME", section_name(path))
    };
    format!("UID FETCH {uid} (BODY.PEEK[{section}])")
}

fn nested_section_mime_fetch_command(uid: u32, path: &[usize]) -> Option<String> {
    (!path.is_empty()).then(|| section_mime_fetch_command(uid, path))
}

#[derive(Debug, Clone)]
enum ImapBodyValue {
    Atom(String),
    String(String),
    List(Vec<ImapBodyValue>),
}

impl ImapBodyValue {
    fn atom(&self) -> Option<&str> {
        match self {
            Self::Atom(value) if !value.eq_ignore_ascii_case("NIL") => Some(value),
            Self::String(value) => Some(value),
            _ => None,
        }
    }
    fn list(&self) -> Option<&[ImapBodyValue]> {
        match self {
            Self::List(values) => Some(values),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct MimePart {
    path: Vec<usize>,
    mime_type: String,
    params: BTreeMap<String, String>,
    content_id: Option<String>,
    disposition: Option<String>,
    disposition_params: BTreeMap<String, String>,
    transfer_encoding: String,
    encoded_size: usize,
    children: Vec<MimePart>,
}

impl MimePart {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
    fn filename(&self) -> Option<&str> {
        self.disposition_params
            .get("filename")
            .or_else(|| self.params.get("name"))
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    }
    fn is_explicit_attachment(&self) -> bool {
        self.disposition
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("attachment"))
    }
    fn is_text_body(&self) -> bool {
        self.mime_type.eq_ignore_ascii_case("text/plain")
            || self.mime_type.eq_ignore_ascii_case("text/html")
    }
    fn is_attachment_candidate(&self) -> bool {
        self.is_explicit_attachment()
            || self.filename().is_some()
            || (self.is_leaf() && !self.is_text_body())
    }
    fn is_attached_container(&self) -> bool {
        !self.is_leaf() && self.is_attachment_candidate()
    }
}

#[derive(Debug, Clone)]
struct PlannedAttachment {
    part: MimePart,
    index: usize,
}

#[derive(Debug, Clone)]
struct PlannedTextPart {
    part: MimePart,
    /// Each enclosing multipart/alternative contributes one branch marker.
    /// A successful later branch suppresses only earlier sibling branches;
    /// text leaves in the same selected branch remain visible together.
    alternative_branches: Vec<AlternativeBranch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AlternativeBranch {
    group: usize,
    branch: usize,
    /// Only the first candidate for a representation may select a composite
    /// branch. A decoded footer sibling must not hide a substantive fallback
    /// when the branch's primary body is undecodable.
    decisive: bool,
}

impl PlannedAttachment {
    fn attachment_id(&self, message_id: &str) -> String {
        mime_attachment_id(message_id, &self.part.path)
    }
}

#[derive(Debug, Clone)]
struct SelectivePlan {
    text_parts: Vec<PlannedTextPart>,
    attachments: Vec<PlannedAttachment>,
}

#[cfg(test)]
fn parse_bodystructure(lines: &[String]) -> Result<MimePart> {
    parse_bodystructure_with_literals(lines, &[])
}

fn parse_bodystructure_response(response: &ImapResponse) -> Result<MimePart> {
    parse_bodystructure_with_literals(&response.lines, &response.literals)
}

fn parse_bodystructure_with_literals(
    lines: &[String],
    literals: &[ImapLiteral],
) -> Result<MimePart> {
    let source = imap_transcript_with_literals(lines, literals)?;
    if source.len() > MAX_MIME_HEADER_BYTES {
        bail!("mime_headers_too_large");
    }
    let offset = find_imap_atom_outside_quotes(&source, "BODYSTRUCTURE")
        .context("IMAP server omitted BODYSTRUCTURE")?
        + "BODYSTRUCTURE".len();
    let (value, _) = parse_imap_body_value(source[offset..].trim_start())
        .context("IMAP BODYSTRUCTURE is malformed")?;
    bodystructure_part(&value, Vec::new())
}

fn find_imap_atom_outside_quotes(source: &str, atom: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let atom = atom.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index + atom.len() <= bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            index += 1;
            continue;
        }
        let before_is_atom = index > 0 && bytes[index - 1].is_ascii_alphanumeric();
        let after = index + atom.len();
        let after_is_atom = after < bytes.len() && bytes[after].is_ascii_alphanumeric();
        if !before_is_atom
            && !after_is_atom
            && bytes[index..]
                .get(..atom.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(atom))
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn imap_transcript_with_literals(lines: &[String], literals: &[ImapLiteral]) -> Result<String> {
    let source = lines.join(" ");
    let mut result = String::with_capacity(source.len());
    let mut cursor = 0;
    let mut literal_index = 0;
    while let Some(offset) = source[cursor..].find('{') {
        let start = cursor + offset;
        let Some(end_offset) = source[start..].find('}') else {
            break;
        };
        let end = start + end_offset;
        let marker = &source[start + 1..end];
        if marker.trim_end_matches('+').parse::<usize>().is_err() {
            result.push_str(&source[cursor..end + 1]);
            cursor = end + 1;
            continue;
        }
        let literal = literals
            .get(literal_index)
            .context("IMAP response omitted a declared literal")?;
        result.push_str(&source[cursor..start]);
        result.push('"');
        result.push_str(
            &String::from_utf8_lossy(&literal.bytes)
                .replace('\\', "\\\\")
                .replace('"', "\\\""),
        );
        result.push('"');
        cursor = end + 1;
        literal_index += 1;
    }
    result.push_str(&source[cursor..]);
    if literal_index != literals.len() {
        bail!("IMAP response contains an unassociated literal");
    }
    Ok(result)
}

fn parse_imap_body_value(input: &str) -> Option<(ImapBodyValue, &str)> {
    let mut values_seen = 0usize;
    parse_imap_body_value_inner(input, 0, &mut values_seen)
}

fn parse_imap_body_value_inner<'a>(
    input: &'a str,
    depth: usize,
    values_seen: &mut usize,
) -> Option<(ImapBodyValue, &'a str)> {
    if depth > MAX_MULTIPART_NESTING + 16 || *values_seen >= MAX_MIME_PARTS.saturating_mul(32) {
        return None;
    }
    *values_seen += 1;
    let input = input.trim_start();
    let mut chars = input.chars();
    match chars.next()? {
        '(' => {
            let mut rest = chars.as_str();
            let mut values = Vec::new();
            loop {
                rest = rest.trim_start();
                if let Some(after) = rest.strip_prefix(')') {
                    return Some((ImapBodyValue::List(values), after));
                }
                let (value, after) = parse_imap_body_value_inner(rest, depth + 1, values_seen)?;
                values.push(value);
                rest = after;
            }
        }
        '"' => {
            let mut value = String::new();
            let mut rest = chars.as_str();
            while let Some(character) = rest.chars().next() {
                rest = &rest[character.len_utf8()..];
                match character {
                    '"' => return Some((ImapBodyValue::String(value), rest)),
                    '\\' => {
                        let escaped = rest.chars().next()?;
                        value.push(escaped);
                        rest = &rest[escaped.len_utf8()..];
                    }
                    _ => value.push(character),
                }
            }
            None
        }
        character => {
            let end = input
                .find(|value: char| value.is_ascii_whitespace() || matches!(value, '(' | ')'))
                .unwrap_or(input.len());
            let _ = character;
            (end > 0).then(|| (ImapBodyValue::Atom(input[..end].to_owned()), &input[end..]))
        }
    }
}

fn bodystructure_params(value: Option<&[ImapBodyValue]>) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    let Some(values) = value else {
        return params;
    };
    for pair in values.chunks_exact(2) {
        if let (Some(name), Some(value)) = (pair[0].atom(), pair[1].atom()) {
            params.insert(name.to_ascii_lowercase(), value.to_owned());
        }
    }
    // BODYSTRUCTURE exposes RFC 2231 extended parameters verbatim on many
    // providers.  Normalize the common single-value form so targeted
    // attachment metadata agrees with the complete MIME parser.
    for (extended, plain) in [("filename*", "filename"), ("name*", "name")] {
        let Some(value) = params.get(extended) else {
            continue;
        };
        let encoded = value
            .split_once("''")
            .map(|(_, encoded)| encoded)
            .unwrap_or(value);
        if valid_percent_escapes(encoded) {
            if let Ok(decoded) = percent_decode_str(encoded).decode_utf8() {
                if !decoded.trim().is_empty() {
                    params.insert(plain.to_owned(), decoded.into_owned());
                }
            }
        }
    }
    params
}

fn bodystructure_disposition(values: &[ImapBodyValue]) -> Option<&[ImapBodyValue]> {
    values
        .iter()
        .filter_map(ImapBodyValue::list)
        .find(|values| {
            values
                .first()
                .and_then(ImapBodyValue::atom)
                .is_some_and(|value| {
                    matches!(value.to_ascii_lowercase().as_str(), "attachment" | "inline")
                })
        })
}

fn bodystructure_part(value: &ImapBodyValue, path: Vec<usize>) -> Result<MimePart> {
    let mut part_count = 0usize;
    bodystructure_part_inner(value, path, 0, &mut part_count)
}

fn bodystructure_part_inner(
    value: &ImapBodyValue,
    path: Vec<usize>,
    depth: usize,
    part_count: &mut usize,
) -> Result<MimePart> {
    if depth > MAX_MULTIPART_NESTING {
        bail!("mime_multipart_nesting_too_deep");
    }
    *part_count = part_count
        .checked_add(1)
        .context("MIME part count overflow")?;
    if *part_count > MAX_MIME_PARTS {
        bail!("mime_too_many_parts");
    }
    let values = value.list().context("BODYSTRUCTURE part is not a list")?;
    if values.first().is_some_and(|value| value.list().is_some()) {
        let child_count = values
            .iter()
            .take_while(|value| value.list().is_some())
            .count();
        let subtype = values
            .get(child_count)
            .and_then(ImapBodyValue::atom)
            .context("multipart BODYSTRUCTURE omitted subtype")?;
        let mut children = Vec::with_capacity(child_count);
        for (index, child) in values[..child_count].iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(index + 1);
            children.push(bodystructure_part_inner(
                child,
                child_path,
                depth + 1,
                part_count,
            )?);
        }
        let disposition_value =
            bodystructure_disposition(values.get(child_count + 2..).unwrap_or_default());
        let disposition = disposition_value
            .and_then(|values| values.first())
            .and_then(ImapBodyValue::atom)
            .map(str::to_ascii_lowercase);
        let disposition_params = disposition_value
            .and_then(|values| values.get(1))
            .and_then(ImapBodyValue::list)
            .map(|values| bodystructure_params(Some(values)))
            .unwrap_or_default();
        return Ok(MimePart {
            path,
            mime_type: format!("multipart/{subtype}").to_ascii_lowercase(),
            params: bodystructure_params(values.get(child_count + 1).and_then(ImapBodyValue::list)),
            content_id: None,
            disposition,
            disposition_params,
            transfer_encoding: String::new(),
            encoded_size: 0,
            children,
        });
    }
    let kind = values
        .first()
        .and_then(ImapBodyValue::atom)
        .context("BODYSTRUCTURE omitted media type")?;
    let subtype = values
        .get(1)
        .and_then(ImapBodyValue::atom)
        .context("BODYSTRUCTURE omitted media subtype")?;
    let disposition_value = bodystructure_disposition(values.get(7..).unwrap_or_default());
    let disposition = disposition_value
        .and_then(|values| values.first())
        .and_then(ImapBodyValue::atom)
        .map(str::to_ascii_lowercase);
    let disposition_params = disposition_value
        .and_then(|values| values.get(1))
        .and_then(ImapBodyValue::list)
        .map(|values| bodystructure_params(Some(values)))
        .unwrap_or_default();
    Ok(MimePart {
        path,
        mime_type: format!("{kind}/{subtype}").to_ascii_lowercase(),
        params: bodystructure_params(values.get(2).and_then(ImapBodyValue::list)),
        content_id: values
            .get(3)
            .and_then(ImapBodyValue::atom)
            .map(str::to_owned),
        disposition,
        disposition_params,
        transfer_encoding: values
            .get(5)
            .and_then(ImapBodyValue::atom)
            .unwrap_or_default()
            .to_ascii_lowercase(),
        encoded_size: values
            .get(6)
            .and_then(ImapBodyValue::atom)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        children: Vec::new(),
    })
}

fn selective_plan(root: &MimePart) -> Result<SelectivePlan> {
    let mut text_parts = Vec::new();
    let mut fallback_groups = 0usize;
    select_text_parts(root, &mut text_parts, &mut fallback_groups);
    let mut candidates = Vec::new();
    collect_attachment_candidates(root, &mut candidates);
    if candidates.len() > MAX_ATTACHMENT_COUNT {
        bail!("message has too many attachments");
    }
    Ok(SelectivePlan {
        text_parts,
        attachments: candidates
            .into_iter()
            .enumerate()
            .map(|(index, part)| PlannedAttachment { part, index })
            .collect(),
    })
}

fn select_text_parts(
    part: &MimePart,
    selected: &mut Vec<PlannedTextPart>,
    alternative_groups: &mut usize,
) {
    if part.is_attached_container() {
        return;
    }
    if part.is_leaf() {
        if part.is_text_body() && !part.is_attachment_candidate() {
            selected.push(PlannedTextPart {
                part: part.clone(),
                alternative_branches: Vec::new(),
            });
        }
        return;
    }
    if part.mime_type.eq_ignore_ascii_case("multipart/alternative") {
        *alternative_groups += 1;
        let group = *alternative_groups;
        // RFC multipart/alternative orders direct children from least to most
        // faithful. Plan the preferred branch first, but do not flatten its
        // nested mixed/related siblings into competing alternatives.
        for (branch, child) in part.children.iter().enumerate().rev() {
            let mut candidates = Vec::new();
            select_text_parts(child, &mut candidates, alternative_groups);
            let mut first_plain = true;
            let mut first_html = true;
            for candidate in &mut candidates {
                let first_for_representation =
                    if candidate.part.mime_type.eq_ignore_ascii_case("text/html") {
                        std::mem::take(&mut first_html)
                    } else {
                        std::mem::take(&mut first_plain)
                    };
                // A nested alternative may legitimately fall back from its
                // first leaf. Propagate each nested branch's primary body to
                // the enclosing branch, while plain mixed siblings still use
                // only their first body as the decisive candidate.
                let decisive = if candidate.alternative_branches.is_empty() {
                    first_for_representation
                } else {
                    candidate
                        .alternative_branches
                        .iter()
                        .all(|branch| branch.decisive)
                };
                candidate.alternative_branches.push(AlternativeBranch {
                    group,
                    branch,
                    decisive,
                });
            }
            selected.extend(candidates);
        }
    } else if part.mime_type.eq_ignore_ascii_case("multipart/related") {
        let declared_start = part
            .params
            .get("start")
            .map(|value| value.trim().trim_matches(['<', '>']).to_ascii_lowercase());
        let root = declared_start
            .as_deref()
            .and_then(|start| {
                part.children.iter().find(|child| {
                    child
                        .content_id
                        .as_deref()
                        .map(|value| {
                            value
                                .trim()
                                .trim_matches(['<', '>'])
                                .eq_ignore_ascii_case(start)
                        })
                        .unwrap_or(false)
                })
            })
            .or_else(|| part.children.first());
        if let Some(root) = root {
            select_text_parts(root, selected, alternative_groups);
        }
    } else {
        for child in &part.children {
            select_text_parts(child, selected, alternative_groups);
        }
    }
}

fn alternative_branch_is_eligible(
    part: &PlannedTextPart,
    successful_branches: &BTreeMap<(usize, bool), usize>,
) -> bool {
    let html = part.part.mime_type.eq_ignore_ascii_case("text/html");
    part.alternative_branches.iter().all(|branch| {
        successful_branches
            .get(&(branch.group, html))
            .is_none_or(|selected| *selected == branch.branch)
    })
}

fn record_successful_alternative_branches(
    part: &PlannedTextPart,
    successful_branches: &mut BTreeMap<(usize, bool), usize>,
) {
    let html = part.part.mime_type.eq_ignore_ascii_case("text/html");
    for branch in &part.alternative_branches {
        if branch.decisive {
            successful_branches
                .entry((branch.group, html))
                .or_insert(branch.branch);
        }
    }
}

fn alternative_branch_is_selected(
    part: &PlannedTextPart,
    successful_branches: &BTreeMap<(usize, bool), usize>,
) -> bool {
    let html = part.part.mime_type.eq_ignore_ascii_case("text/html");
    part.alternative_branches.iter().all(|branch| {
        successful_branches
            .get(&(branch.group, html))
            .is_none_or(|selected| *selected == branch.branch)
    })
}

fn collect_attachment_candidates(part: &MimePart, candidates: &mut Vec<MimePart>) {
    if part.is_attached_container() {
        candidates.push(part.clone());
        return;
    }
    if part.is_leaf() {
        if part.is_attachment_candidate() {
            candidates.push(part.clone());
        }
        return;
    }
    for child in &part.children {
        collect_attachment_candidates(child, candidates);
    }
}

async fn fetch_section_mime_headers<S>(
    client: &mut ImapClient<S>,
    uid: u32,
    path: &[usize],
    root_headers: Option<&[u8]>,
) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if path.is_empty() {
        return root_headers
            .map(ToOwned::to_owned)
            .context("IMAP root MIME headers were not fetched");
    }
    let command = nested_section_mime_fetch_command(uid, path)
        .context("IMAP MIME header section is missing")?;
    let mut response = client
        .command_with_literal_limited(&command, 256 * 1024)
        .await?;
    response
        .take_body_literal_for(uid, &format!("{}.MIME", section_name(path)))
        .context("IMAP server did not return MIME part headers")
}

async fn fetch_selective_message<S>(
    client: &mut ImapClient<S>,
    account: &Account,
    mailbox: &str,
    uid: u32,
    response_lines: &[String],
    headers: &[u8],
    structure: &MimePart,
) -> Result<MailSummary>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let plan = selective_plan(structure)?;
    let mut total = 0usize;
    let mut decoded_text_parts = Vec::new();
    let mut text_decode_failures = 0usize;
    let mut successful_alternative_branches = BTreeMap::new();
    let mut referenced_inline = BTreeMap::new();
    for part in &plan.text_parts {
        if !alternative_branch_is_eligible(part, &successful_alternative_branches) {
            continue;
        }
        let header =
            fetch_section_mime_headers(client, uid, &part.part.path, Some(headers)).await?;
        let remaining =
            display_literal_limit(total, part.part.encoded_size, "message display part")?;
        let mut response = client
            .command_with_literal_limited(
                &section_fetch_command(uid, &part.part.path),
                remaining.min(MAX_DISPLAY_PART_BYTES),
            )
            .await?;
        let bytes = response
            .take_body_literal_for(uid, &response_body_section(&part.part.path))
            .context("IMAP server did not return the requested MIME part")?;
        total = total
            .checked_add(bytes.len())
            .context("message display body size overflow")?;
        let decoded = match decode_mime_part_body(&header, &bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                text_decode_failures += 1;
                tracing::warn!(
                    %error,
                    uid,
                    section = %section_name(&part.part.path),
                    "could not decode a selective MIME body candidate"
                );
                continue;
            }
        };
        record_successful_alternative_branches(part, &mut successful_alternative_branches);
        decoded_text_parts.push((part.clone(), decoded));
    }
    let mut plain = Vec::new();
    let mut html = Vec::new();
    for (part, decoded) in decoded_text_parts {
        if !alternative_branch_is_selected(&part, &successful_alternative_branches) {
            continue;
        }
        if part.part.mime_type.eq_ignore_ascii_case("text/html") {
            html.push(decoded);
        } else if part.part.mime_type.eq_ignore_ascii_case("text/plain") {
            plain.push(decoded);
        }
    }
    let mut image_parts = BTreeMap::new();
    let mut reference_counts = BTreeMap::new();
    for attachment in &plan.attachments {
        if !attachment.part.mime_type.starts_with("image/") {
            for reference in unique_mime_part_references(&attachment.part) {
                *reference_counts
                    .entry(reference.to_ascii_lowercase())
                    .or_insert(0usize) += 1;
            }
            continue;
        }
        let header =
            fetch_section_mime_headers(client, uid, &attachment.part.path, Some(headers)).await?;
        let part = mime_part_with_headers(&attachment.part, &header)?;
        let references = unique_mime_part_references(&part);
        for reference in &references {
            *reference_counts
                .entry(reference.to_ascii_lowercase())
                .or_insert(0usize) += 1;
        }
        image_parts.insert(section_name(&part.path), (part, header, references));
    }
    let mut body_html = join_visible_html_segments(html);
    if let Some(html) = &mut body_html {
        let html_references = html_resource_references(html);
        let mut replacements = BTreeMap::new();
        for (path, (part, header, references)) in &image_parts {
            let referenced =
                has_unambiguous_html_reference(&html_references, references, &reference_counts);
            referenced_inline.insert(path.clone(), referenced);
            if !referenced {
                continue;
            }
            let remaining = display_literal_limit(total, part.encoded_size, "inline image")?;
            let mut response = client
                .command_with_literal_limited(
                    &section_fetch_command(uid, &part.path),
                    remaining.min(MAX_DISPLAY_PART_BYTES),
                )
                .await?;
            let bytes = response
                .take_body_literal_for(uid, &response_body_section(&part.path))
                .context("IMAP server did not return the referenced inline image")?;
            total = total
                .checked_add(bytes.len())
                .context("message display body size overflow")?;
            let image = decode_mime_part_raw(header, &bytes)?;
            let data_url = format!(
                "data:{};base64,{}",
                safe_mime_type(&part.mime_type),
                STANDARD.encode(image)
            );
            for reference in references {
                replacements
                    .entry(reference.to_ascii_lowercase())
                    .or_insert_with(|| data_url.clone());
            }
        }
        *html = rewrite_html_resource_references(html, &replacements)?;
    }
    let mut body_text = plain
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body_text.trim().is_empty() {
        if let Some(html) = &body_html {
            body_text = mail_parser::decoders::html::html_to_text(html);
        }
    }
    if !plan.text_parts.is_empty()
        && text_decode_failures == plan.text_parts.len()
        && body_text.trim().is_empty()
        && body_html.is_none()
    {
        bail!(MIME_CONTENT_UNDECODABLE);
    }
    let id = stable_message_id(account.id, mailbox, uid);
    let mut attachments = Vec::new();
    for attachment in &plan.attachments {
        let part = image_parts
            .get(&section_name(&attachment.part.path))
            .map(|(part, _, _)| part.clone())
            .unwrap_or_else(|| attachment.part.clone());
        let referenced = referenced_inline
            .get(&section_name(&part.path))
            .copied()
            .unwrap_or(false);
        let data = attachment_data_from_part(
            &PlannedAttachment {
                part,
                index: attachment.index,
            },
            &id,
            &[],
            None,
            referenced,
        )?;
        if data.attachment.presentation.is_downloadable() {
            attachments.push(data);
        }
    }
    let mut message = parse_catalog_message(
        account,
        mailbox,
        uid,
        response_lines,
        headers,
        clean_snippet(&body_text),
    )?;
    message.body_text = body_text;
    message.body_html = body_html;
    message.content_state = "complete".into();
    message.has_attachments = !attachments.is_empty();
    message.attachments = attachments;
    Ok(message)
}

fn display_literal_limit(total: usize, advertised_size: usize, label: &str) -> Result<usize> {
    let remaining = MAX_DISPLAY_TOTAL_BYTES
        .checked_sub(total)
        .context("message display body exceeds the 50 MiB safety limit")?;
    if advertised_size > remaining || advertised_size > MAX_DISPLAY_PART_BYTES {
        bail!(
            "{label} exceeds the {} MiB safety limit",
            MAX_DISPLAY_PART_BYTES / 1024 / 1024
        );
    }
    Ok(remaining.min(MAX_DISPLAY_PART_BYTES))
}

fn validate_targeted_attachment_bytes(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        bail!(
            "attachment exceeds the {} MiB safety limit",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        );
    }
    Ok(())
}

fn decode_mime_part_raw(headers: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    let mut source = Vec::with_capacity(headers.len() + bytes.len() + 4);
    source.extend_from_slice(headers);
    finish_mime_headers(&mut source);
    source.extend_from_slice(bytes);
    let parsed = parse_complete_message(&source)?;
    let part = parsed
        .part(0)
        .context("MIME part parser omitted the root part")?;
    decoded_part_bytes(parsed.raw_message(), part)
        .context("MIME part uses an unsupported transfer encoding")
}

fn decode_mime_part_body(headers: &[u8], bytes: &[u8]) -> Result<String> {
    let mut source = Vec::with_capacity(headers.len() + bytes.len() + 4);
    source.extend_from_slice(headers);
    finish_mime_headers(&mut source);
    source.extend_from_slice(bytes);
    let parsed = parse_complete_message(&source)?;
    let part = parsed
        .part(0)
        .context("MIME part parser omitted the root part")?;
    decode_text_candidate(&parsed, part)
        .map(|text| {
            if part.is_text_html() {
                text
            } else {
                flowed_text(part, &text)
            }
        })
        .map_err(|()| anyhow::anyhow!("MIME text part could not be decoded safely"))
}

fn mime_part_with_headers(part: &MimePart, headers: &[u8]) -> Result<MimePart> {
    let mut source = headers.to_vec();
    finish_mime_headers(&mut source);
    let parsed = configured_message_parser()
        .parse_headers(&source)
        .context("MIME part headers are malformed")?;
    let parsed = parsed
        .part(0)
        .context("MIME part parser omitted the root part")?;
    let mut result = part.clone();
    if let Some(content_type) = parsed.content_type() {
        if let Some(subtype) = content_type.subtype() {
            result.mime_type = format!("{}/{}", content_type.ctype(), subtype).to_ascii_lowercase();
        }
        if let Some(attributes) = content_type.attributes() {
            result.params.extend(attributes.iter().map(|attribute| {
                (
                    attribute.name.to_ascii_lowercase(),
                    attribute.value.to_string(),
                )
            }));
        }
    }
    result.content_id = parsed
        .content_id()
        .map(ToOwned::to_owned)
        .or(result.content_id);
    if let Some(disposition) = parsed.content_disposition() {
        result.disposition = Some(
            if disposition.is_attachment() {
                "attachment"
            } else if disposition.is_inline() {
                "inline"
            } else {
                ""
            }
            .to_owned(),
        );
        if let Some(attributes) = disposition.attributes() {
            result
                .disposition_params
                .extend(attributes.iter().map(|attribute| {
                    (
                        attribute.name.to_ascii_lowercase(),
                        attribute.value.to_string(),
                    )
                }));
        }
    }
    if let Some(location) = parsed.content_location() {
        result
            .params
            .insert("content-location".into(), location.to_owned());
    }
    Ok(result)
}

fn finish_mime_headers(source: &mut Vec<u8>) {
    if source.ends_with(b"\r\n\r\n") || source.ends_with(b"\n\n") {
        return;
    }
    if source.ends_with(b"\r\n") {
        source.extend_from_slice(b"\r\n");
    } else if source.ends_with(b"\n") {
        source.extend_from_slice(b"\n");
    } else {
        source.extend_from_slice(b"\r\n\r\n");
    }
}

fn mime_part_references(part: &MimePart) -> Vec<String> {
    let mut references = Vec::new();
    if let Some(content_id) = &part.content_id {
        references.push(format!(
            "cid:{}",
            content_id.trim().trim_matches(['<', '>'])
        ));
    }
    if let Some(location) = part.params.get("content-location") {
        references.push(location.to_owned());
    }
    references
}

fn unique_mime_part_references(part: &MimePart) -> Vec<String> {
    let mut seen = BTreeSet::new();
    mime_part_references(part)
        .into_iter()
        .filter(|reference| seen.insert(reference.to_ascii_lowercase()))
        .collect()
}

fn has_unambiguous_html_reference(
    html_references: &BTreeSet<String>,
    references: &[String],
    reference_counts: &BTreeMap<String, usize>,
) -> bool {
    references.iter().any(|reference| {
        html_references.contains(&reference.to_ascii_lowercase())
            && reference_counts
                .get(&reference.to_ascii_lowercase())
                .copied()
                == Some(1)
    })
}

fn attachment_data_from_part(
    part: &PlannedAttachment,
    message_id: &str,
    headers: &[u8],
    bytes: Option<Vec<u8>>,
    referenced: bool,
) -> Result<AttachmentData> {
    let planned = part;
    let part = if headers.is_empty() {
        planned.part.clone()
    } else {
        mime_part_with_headers(&planned.part, headers)?
    };
    let filename = safe_attachment_filename(part.filename().unwrap_or("attachment"), planned.index);
    let mime_type = safe_mime_type(&part.mime_type);
    let is_inline = part
        .disposition
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("inline"))
        || part.content_id.is_some();
    let presentation = match (part.is_explicit_attachment(), referenced) {
        (true, true) => AttachmentPresentation::Both,
        (true, false) => AttachmentPresentation::Downloadable,
        (false, true) => AttachmentPresentation::Embedded,
        (false, false) => AttachmentPresentation::Downloadable,
    };
    let bytes = match bytes {
        Some(bytes) => decode_mime_part_raw(headers, &bytes)?,
        None => Vec::new(),
    };
    Ok(AttachmentData {
        attachment: Attachment {
            id: planned.attachment_id(message_id),
            message_id: message_id.to_owned(),
            filename: filename.clone(),
            mime_type: mime_type.clone(),
            size_bytes: if bytes.is_empty() {
                part.encoded_size as i64
            } else {
                bytes.len() as i64
            },
            is_inline,
            presentation,
            is_potentially_unsafe: is_potentially_unsafe(&filename, &mime_type),
        },
        bytes,
    })
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

impl ImapClient<TlsStream<TcpStream>> {
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
            let greeting = timeout(
                IMAP_COMMAND_TIMEOUT,
                read_imap_line_limited(&mut plain, MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES),
            )
            .await
            .context("IMAP greeting timed out")??;
            if !greeting.starts_with("* OK") {
                bail!("IMAP server rejected connection: {}", greeting.trim());
            }
            plain.get_mut().write_all(b"D0000 STARTTLS\r\n").await?;
            plain.get_mut().flush().await?;
            let response = timeout(
                IMAP_COMMAND_TIMEOUT,
                read_imap_line_limited(&mut plain, MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES),
            )
            .await
            .context("IMAP STARTTLS response timed out")??;
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
            let greeting = timeout(
                IMAP_COMMAND_TIMEOUT,
                read_imap_line_limited(&mut reader, MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES),
            )
            .await
            .context("IMAP greeting timed out")??;
            if !greeting.starts_with("* OK") {
                bail!("IMAP server rejected connection: {}", greeting.trim());
            }
        }
        Ok(Self { reader, tag: 0 })
    }
}

// Keeping command framing generic lets the protocol state machine be driven
// by a deterministic local socket in tests. Production construction remains
// the Rustls-only `ImapClient::connect` implementation above.
impl<S> ImapClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn authenticate(&mut self, account: &Account, secret: &str) -> Result<()> {
        let result = match &account.auth {
            AccountAuth::Password { username } => self
                .command(&format!(
                    "LOGIN {} {}",
                    quote_imap(username),
                    quote_imap(secret)
                ))
                .await
                .map(|_| ()),
            AccountAuth::OAuth2 { username, .. } => {
                let auth =
                    STANDARD.encode(format!("user={username}\x01auth=Bearer {secret}\x01\x01"));
                self.command(&format!("AUTHENTICATE XOAUTH2 {auth}"))
                    .await
                    .map(|_| ())
            }
        };
        if let Err(error) = result {
            // A tagged NO/BAD while executing the authentication command is
            // an authentication rejection. BYE, EOF, timeout, and other
            // transport failures must remain retryable; callers use the
            // authentication wording to decide whether realtime should pause.
            if error.to_string().starts_with("IMAP command failed:") {
                return Err(error).context("IMAP authentication rejected");
            }
            return Err(error);
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
            let read = timeout(
                IMAP_COMMAND_TIMEOUT,
                self.reader.read_line(&mut continuation),
            )
            .await
            .context("IMAP IDLE continuation timed out")??;
            if read == 0 {
                bail!("IMAP connection closed before IDLE continuation");
            }
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
            let read = timeout(IMAP_COMMAND_TIMEOUT, self.reader.read_line(&mut line))
                .await
                .context("IMAP IDLE termination timed out")??;
            if read == 0 {
                bail!("IMAP connection closed during IDLE termination");
            }
            if line.to_ascii_uppercase().starts_with("* BYE") {
                bail!("IMAP server closed the connection during IDLE termination");
            }
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
        self.command_with_literal_limited(command, MAX_IMAP_RESPONSE_LITERAL_BYTES)
            .await
    }

    async fn command_with_literal_limited(
        &mut self,
        command: &str,
        max_literal_bytes: usize,
    ) -> Result<ImapResponse> {
        timeout(
            IMAP_COMMAND_TIMEOUT,
            self.command_with_literal_inner(command, max_literal_bytes),
        )
        .await
        .context("IMAP command timed out")?
    }

    async fn command_with_literal_inner(
        &mut self,
        command: &str,
        max_literal_bytes: usize,
    ) -> Result<ImapResponse> {
        self.tag += 1;
        let tag = format!("D{:04}", self.tag);
        self.reader
            .get_mut()
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .await?;
        self.reader.get_mut().flush().await?;
        let mut lines = Vec::new();
        let mut literals = Vec::new();
        let mut literal_bytes = 0usize;
        let mut transcript_bytes = 0usize;
        // A FETCH response may continue after a literal on a line that no
        // longer repeats UID. Retain the owner until a new untagged FETCH
        // starts; a new one replaces it, so unsolicited interleaving cannot
        // donate its literal to the requested message.
        let mut current_fetch_uid = None;
        let mut pending_fetch_literals = Vec::new();
        loop {
            let line = self
                .read_command_line_limited(MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES)
                .await?;
            transcript_bytes = reserve_imap_transcript_budget(transcript_bytes, line.len())?;
            let upper = line.trim_start().to_ascii_uppercase();
            let is_bye = upper.strip_prefix("* BYE").is_some_and(|suffix| {
                suffix.is_empty()
                    || suffix
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_whitespace)
            });
            if is_bye {
                bail!("IMAP server closed the connection: {}", line.trim());
            }
            if is_untagged_fetch_start(&line) {
                current_fetch_uid = None;
                pending_fetch_literals.clear();
            }
            if let Some(uid) = fetch_uid_from_line(&line) {
                current_fetch_uid = Some(uid);
                backfill_fetch_literal_uids(&mut literals, &mut pending_fetch_literals, uid);
            }
            lines.push(line.clone());
            if let Some(length) = literal_length(&line) {
                if length > MAX_RAW_MESSAGE_BYTES {
                    bail!("mime_raw_message_too_large");
                }
                reserve_imap_literal_budget(
                    literals.len(),
                    literal_bytes,
                    length,
                    max_literal_bytes,
                )?;
                let mut bytes = vec![0; length];
                self.reader.read_exact(&mut bytes).await?;
                literal_bytes += length;
                let pending = current_fetch_uid.is_none();
                literals.push(ImapLiteral {
                    uid: current_fetch_uid,
                    data_item: literal_data_item(&line),
                    bytes,
                });
                if pending {
                    pending_fetch_literals.push(literals.len() - 1);
                }
            }
            if line.starts_with(&tag) {
                if !line[tag.len()..].trim_start().starts_with("OK") {
                    bail!("IMAP command failed: {}", line.trim());
                }
                break;
            }
        }
        Ok(ImapResponse { lines, literals })
    }

    async fn read_command_line_limited(&mut self, max_bytes: usize) -> Result<String> {
        read_imap_line_limited(&mut self.reader, max_bytes)
            .await
            .context("IMAP connection closed during command")
    }
}

async fn read_imap_line_limited<R>(reader: &mut R, max_bytes: usize) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if bytes.len() >= max_bytes {
            bail!("IMAP response line exceeds MIME safety limit");
        }
        bytes.push(byte);
        if byte == b'\n' {
            return String::from_utf8(bytes).context("IMAP response line is not valid UTF-8");
        }
    }
}

fn fetch_uid_from_line(line: &str) -> Option<u32> {
    let bytes = line.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            index += 1;
            continue;
        }
        if bytes[index..]
            .get(..4)
            .is_some_and(|token| token.eq_ignore_ascii_case(b"UID "))
        {
            let start = index + 4;
            let end = bytes[start..]
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .map(|offset| start + offset)
                .unwrap_or(bytes.len());
            return (end > start)
                .then(|| line[start..end].parse().ok())
                .flatten();
        }
        index += 1;
    }
    None
}

fn is_untagged_fetch_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('*') && trimmed.to_ascii_uppercase().contains(" FETCH ")
}

fn backfill_fetch_literal_uids(literals: &mut [ImapLiteral], pending: &mut Vec<usize>, uid: u32) {
    for index in pending.drain(..) {
        literals[index].uid = Some(uid);
    }
}

fn reserve_imap_literal_budget(
    current_count: usize,
    current_bytes: usize,
    next_bytes: usize,
    max_total_bytes: usize,
) -> Result<()> {
    if current_count >= MAX_IMAP_RESPONSE_LITERALS {
        bail!("IMAP response exceeds the {MAX_IMAP_RESPONSE_LITERALS} literal safety limit");
    }
    let total = current_bytes
        .checked_add(next_bytes)
        .context("IMAP literal byte count overflow")?;
    if next_bytes > max_total_bytes || total > max_total_bytes {
        bail!(
            "IMAP response literals exceed the {} MiB safety limit",
            max_total_bytes / 1024 / 1024
        );
    }
    Ok(())
}

fn reserve_imap_transcript_budget(current_bytes: usize, next_bytes: usize) -> Result<usize> {
    let total = current_bytes
        .checked_add(next_bytes)
        .context("IMAP response transcript size overflow")?;
    if total > MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES {
        bail!("IMAP response transcript exceeds MIME safety limit");
    }
    Ok(total)
}

fn literal_data_item(line: &str) -> ImapLiteralItem {
    let bytes = line.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    let mut latest = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            index += 1;
            continue;
        }
        let remaining = &bytes[index..];
        if remaining
            .get(.."BODYSTRUCTURE".len())
            .is_some_and(|value| value.eq_ignore_ascii_case(b"BODYSTRUCTURE"))
        {
            latest = Some(ImapLiteralItem::BodyStructure);
            index += "BODYSTRUCTURE".len();
            continue;
        }
        if remaining
            .get(.."BODY[".len())
            .is_some_and(|value| value.eq_ignore_ascii_case(b"BODY["))
        {
            let end = remaining
                .iter()
                .position(|byte| *byte == b']')
                .map(|offset| index + offset + 1)
                .unwrap_or(bytes.len());
            latest = Some(ImapLiteralItem::Body(
                String::from_utf8_lossy(&bytes[index..end]).into_owned(),
            ));
            index = end;
            continue;
        }
        index += 1;
    }
    latest.unwrap_or(ImapLiteralItem::Other)
}

fn literal_length(line: &str) -> Option<usize> {
    let line = line.trim_end_matches(['\r', '\n']);
    if !line.ends_with('}') {
        return None;
    }
    let start = line.rfind('{')? + 1;
    let length = line[start..line.len() - 1].trim_end_matches('+');
    (!length.is_empty() && length.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| length.parse().ok())
        .flatten()
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

fn verify_mailbox_uid_validity(current: u32, catalogued: i64, action: &str) -> Result<()> {
    if i64::from(current) != catalogued {
        bail!("mailbox identity changed; sync the account before {action} this message");
    }
    Ok(())
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
            html: { join_visible_html_segments(self.html) },
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
        // RFC multipart/alternative orders direct children by increasing
        // faithfulness. Keep the latest direct branch for each representation,
        // but retain every mixed/related text sibling inside that branch.
        if !candidate.plain.is_empty() {
            chosen_plain = Some(candidate.plain);
        }
        if !candidate.html.is_empty() {
            chosen_html = Some(candidate.html);
        }
        selected.valid_candidates += candidate.valid_candidates;
        selected.undecodable_candidates += candidate.undecodable_candidates;
    }
    if let Some(plain) = chosen_plain {
        selected.plain.extend(plain);
    }
    if let Some(html) = chosen_html {
        selected.html.extend(html);
    }
    selected
}

fn join_visible_html_segments(segments: Vec<String>) -> Option<String> {
    let selected = segments
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>();
    (!selected.is_empty()).then(|| selected.join("\n"))
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
    html: String,
    mail: &ParsedMessage<'_>,
    raw: &[u8],
) -> Result<String> {
    let inline_images = collect_inline_images(mail, raw, &html);
    let mut replacements = BTreeMap::new();
    for (reference, data_url) in inline_images {
        replacements
            .entry(reference.to_ascii_lowercase())
            .or_insert(data_url);
    }
    rewrite_html_resource_references(&html, &replacements)
}

fn collect_inline_images(
    mail: &ParsedMessage<'_>,
    raw: &[u8],
    html: &str,
) -> Vec<(String, String)> {
    let html_references = html_resource_references(html);
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
                !reference.is_empty() && html_references.contains(&reference.to_ascii_lowercase())
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

#[cfg(test)]
fn replace_resource_reference(input: &str, reference: &str, replacement: &str) -> Result<String> {
    rewrite_html_resource_references(
        input,
        &BTreeMap::from([(reference.to_ascii_lowercase(), replacement.to_owned())]),
    )
}

fn html_contains_reference(html: &str, reference: &str) -> bool {
    html_resource_references(html).contains(&reference.to_ascii_lowercase())
}

fn html_resource_references(input: &str) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    let mut cursor = 0;
    while let Some(offset) = input[cursor..].find('<') {
        let start = cursor + offset;
        if input[start..].starts_with("<!--") {
            cursor = input[start + 4..]
                .find("-->")
                .map_or(input.len(), |offset| start + 4 + offset + 3);
            continue;
        }
        let Some(end) = html_tag_end(input, start) else {
            break;
        };
        collect_html_tag_resource_references(&input[start..end], &mut references);
        cursor = raw_text_element_end(input, start, end).unwrap_or(end);
    }
    references
}

fn rewrite_html_resource_references(
    input: &str,
    replacements: &BTreeMap<String, String>,
) -> Result<String> {
    if replacements.is_empty() {
        return Ok(input.to_owned());
    }
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(offset) = input[cursor..].find('<') {
        let start = cursor + offset;
        push_resolved_html(&mut output, &input[cursor..start])?;
        if input[start..].starts_with("<!--") {
            let end = input[start + 4..]
                .find("-->")
                .map_or(input.len(), |offset| start + 4 + offset + 3);
            push_resolved_html(&mut output, &input[start..end])?;
            cursor = end;
            continue;
        }
        let Some(end) = html_tag_end(input, start) else {
            push_resolved_html(&mut output, &input[start..])?;
            return Ok(output);
        };
        let tag = &input[start..end];
        push_resolved_html(
            &mut output,
            &rewrite_resource_references_in_tag(tag, replacements)?,
        )?;
        if let Some(raw_text_end) = raw_text_element_end(input, start, end) {
            push_resolved_html(&mut output, &input[end..raw_text_end])?;
            cursor = raw_text_end;
        } else {
            cursor = end;
        }
    }
    push_resolved_html(&mut output, &input[cursor..])?;
    Ok(output)
}

fn push_resolved_html(output: &mut String, value: &str) -> Result<()> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > MAX_RAW_MESSAGE_BYTES)
    {
        bail!("mime_resolved_html_too_large");
    }
    output.push_str(value);
    Ok(())
}

fn html_tag_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut quote = None;
    for (index, byte) in bytes.iter().enumerate().skip(start + 1) {
        match quote {
            Some(current) if *byte == current => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"') => quote = Some(*byte),
            None if *byte == b'>' => return Some(index + 1),
            None => {}
        }
    }
    None
}

fn raw_text_element_end(input: &str, start: usize, opening_end: usize) -> Option<usize> {
    let after_open = input.get(start + 1..opening_end)?;
    let name_end = after_open
        .find(|character: char| character.is_ascii_whitespace() || matches!(character, '/' | '>'))
        .unwrap_or(after_open.len());
    let name = &after_open[..name_end];
    if ![
        "script",
        "style",
        "textarea",
        "title",
        "xmp",
        "iframe",
        "noembed",
        "noframes",
        "plaintext",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        return None;
    }
    if name.eq_ignore_ascii_case("plaintext") {
        return Some(input.len());
    }

    let closing = format!("</{name}");
    let remainder = input.get(opening_end..)?;
    let offset = remainder
        .as_bytes()
        .windows(closing.len())
        .enumerate()
        .find(|(offset, window)| {
            window.eq_ignore_ascii_case(closing.as_bytes())
                && remainder
                    .as_bytes()
                    .get(offset + closing.len())
                    .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        })
        .map(|(offset, _)| offset);
    let Some(closing_start) = offset.map(|offset| opening_end + offset) else {
        return Some(input.len());
    };
    Some(html_tag_end(input, closing_start).unwrap_or(input.len()))
}

fn rewrite_resource_references_in_tag(
    tag: &str,
    replacements: &BTreeMap<String, String>,
) -> Result<String> {
    let mut ranges = Vec::new();
    collect_html_tag_resource_ranges(tag, |range, value| {
        if let Some(replacement) = replacements.get(&value.to_ascii_lowercase()) {
            ranges.push((range, replacement.as_str()));
        }
    });
    if ranges.is_empty() {
        return Ok(tag.to_owned());
    }
    let projected_len = ranges
        .iter()
        .try_fold(tag.len(), |length, (range, replacement)| {
            length
                .checked_add(replacement.len())
                .and_then(|value| value.checked_sub(range.len()))
                .context("mime_resolved_html_too_large")
        })?;
    if projected_len > MAX_RAW_MESSAGE_BYTES {
        bail!("mime_resolved_html_too_large");
    }
    let mut output = String::with_capacity(projected_len);
    let mut cursor = 0;
    for (range, replacement) in ranges {
        output.push_str(&tag[cursor..range.start]);
        output.push_str(replacement);
        cursor = range.end;
    }
    output.push_str(&tag[cursor..]);
    Ok(output)
}

fn collect_html_tag_resource_references(tag: &str, references: &mut BTreeSet<String>) {
    collect_html_tag_resource_ranges(tag, |_, value| {
        references.insert(value.to_ascii_lowercase());
    });
}

fn collect_html_tag_resource_ranges<'a>(
    tag: &'a str,
    mut found: impl FnMut(Range<usize>, &'a str),
) {
    let bytes = tag.as_bytes();
    let mut index = 1usize;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= bytes.len() || matches!(bytes[index], b'/' | b'!' | b'?') {
        return;
    }
    while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>' {
        index += 1;
    }
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || matches!(bytes[index], b'/' | b'>') {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'/' | b'>')
        {
            index += 1;
        }
        let attribute_name = tag[name_start..index].to_ascii_lowercase();
        let relevant = matches!(
            attribute_name.as_str(),
            "src" | "href" | "background" | "poster" | "data"
        );
        let is_style = attribute_name == "style";
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let (start, end) = if matches!(bytes[index], b'\'' | b'"') {
            let quote = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            (start, index)
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>'
            {
                index += 1;
            }
            (start, index)
        };
        if relevant && start < end {
            found(start..end, &tag[start..end]);
        }
        if is_style && start < end {
            collect_css_url_resource_ranges(&tag[start..end], |range, value| {
                found(start + range.start..start + range.end, value);
            });
        }
        if index < bytes.len() && matches!(bytes[index], b'\'' | b'"') {
            index += 1;
        }
    }
}

fn collect_css_url_resource_ranges<'a>(tag: &'a str, mut found: impl FnMut(Range<usize>, &'a str)) {
    let bytes = tag.as_bytes();
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        if !bytes[index..index + 4].eq_ignore_ascii_case(b"url(") {
            index += 1;
            continue;
        }
        index += 4;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let quote = matches!(bytes.get(index), Some(b'\'' | b'\"')).then(|| bytes[index]);
        if quote.is_some() {
            index += 1;
        }
        let start = index;
        while index < bytes.len()
            && quote.map_or(bytes[index] != b')', |value| bytes[index] != value)
        {
            index += 1;
        }
        let end = index;
        if let Some(quote) = quote {
            if index >= bytes.len() || bytes[index] != quote {
                break;
            }
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
        }
        if index >= bytes.len() || bytes[index] != b')' {
            break;
        }
        if start < end {
            found(start..end, &tag[start..end]);
        }
        index += 1;
    }
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
    attachment_part_paths(mail)
        .into_iter()
        .map(|(part_id, _)| part_id)
        .collect()
}

fn attachment_part_paths(mail: &ParsedMessage<'_>) -> Vec<(u32, Vec<usize>)> {
    let mut attachments = Vec::new();
    let mut pending = vec![(0_u32, Vec::new())];
    while let Some((part_id, path)) = pending.pop() {
        let Some(part) = mail.part(part_id) else {
            continue;
        };
        if is_attachment_part(mail, part) {
            attachments.push((part_id, path));
            continue;
        }
        if let PartType::Multipart(children) = &part.body {
            pending.extend(children.iter().enumerate().rev().map(|(index, child)| {
                let mut child_path = path.clone();
                child_path.push(index + 1);
                (*child, child_path)
            }));
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
    let candidate_parts = attachment_part_paths(mail);
    if candidate_parts.len() > MAX_ATTACHMENT_COUNT {
        bail!("message has more than {MAX_ATTACHMENT_COUNT} attachments");
    }
    let mut attachments = Vec::new();
    let mut decoded_bytes = 0_usize;
    for (part_id, part_path) in candidate_parts {
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
            id: mime_attachment_id(message_id, &part_path),
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

fn mime_attachment_id(message_id: &str, path: &[usize]) -> String {
    let section = if path.is_empty() {
        "root".to_owned()
    } else {
        section_name(path)
    };
    format!("{message_id}:mime-v1:{section}")
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
    let truncated = truncated.trim().trim_matches('.').trim();
    if truncated.is_empty() {
        format!("attachment-{}", index + 1)
    } else {
        truncated.to_owned()
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
            && !subtype.contains('/')
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
    use lettre::transport::smtp::client::Certificate;
    use tokio::{
        io::{duplex, split, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
        net::{TcpListener, TcpStream},
        sync::watch,
    };
    use tokio_rustls::{
        rustls::{
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
            ServerConfig,
        },
        TlsAcceptor,
    };

    /// A deliberately small transcript harness. The client is the production
    /// command state machine; only the transport is plain loopback TCP so the
    /// fixture can control byte fragmentation without changing TLS policy.
    async fn plain_imap_client_and_server() -> (ImapClient<TcpStream>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (
            ImapClient {
                reader: BufReader::new(client),
                tag: 0,
            },
            server,
        )
    }

    async fn scripted_command<R>(reader: &mut BufReader<R>) -> (String, String)
    where
        R: AsyncRead + Unpin,
    {
        let mut command = String::new();
        reader.read_line(&mut command).await.unwrap();
        let (tag, command) = command.trim_end().split_once(' ').unwrap();
        (tag.to_owned(), command.to_owned())
    }

    async fn scripted_expect_command<R>(
        reader: &mut BufReader<R>,
        transcript: &mut Vec<String>,
        expected: &str,
    ) -> String
    where
        R: AsyncRead + Unpin,
    {
        let (tag, command) = scripted_command(reader).await;
        transcript.push(command.clone());
        assert_eq!(command, expected);
        tag
    }

    async fn write_transcript_fragments(
        writer: &mut tokio::net::tcp::OwnedWriteHalf,
        fragments: &[&[u8]],
    ) {
        for fragment in fragments {
            writer.write_all(fragment).await.unwrap();
            writer.flush().await.unwrap();
        }
    }

    async fn scripted_inbox_sync_server(
        server: TcpStream,
        uid_validity: u32,
        remote_uids: &[u32],
        fetched_uid: u32,
        subject: &str,
    ) -> Vec<String> {
        let (read, mut write) = server.into_split();
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let tag = scripted_expect_command(
            &mut read,
            &mut transcript,
            "LOGIN \"reader@example.test\" \"sync secret\"",
        )
        .await;
        let response = format!("{tag} OK authenticated\r\n");
        write.write_all(response.as_bytes()).await.unwrap();

        let tag = scripted_expect_command(&mut read, &mut transcript, "LIST \"\" \"*\"").await;
        let response = format!("* LIST (\\\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK listed\r\n");
        write.write_all(response.as_bytes()).await.unwrap();

        let tag = scripted_expect_command(&mut read, &mut transcript, "SELECT \"INBOX\"").await;
        let response = format!(
            "* {} EXISTS\r\n* OK [UIDVALIDITY {uid_validity}] UIDs valid\r\n{tag} OK selected\r\n",
            remote_uids.len()
        );
        write.write_all(response.as_bytes()).await.unwrap();

        let tag = scripted_expect_command(&mut read, &mut transcript, "UID SEARCH ALL").await;
        let response = format!(
            "* SEARCH {}\r\n{tag} OK searched\r\n",
            remote_uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        );
        write.write_all(response.as_bytes()).await.unwrap();

        // Keep this exact single exchange in the transcript: a duplicate
        // flags request previously doubled the provider work before download.
        let tag =
            scripted_expect_command(&mut read, &mut transcript, "UID FETCH 1:* (UID FLAGS)").await;
        let flags = remote_uids
            .iter()
            .map(|uid| format!("* 1 FETCH (UID {uid} FLAGS ())\r\n"))
            .collect::<String>();
        let response = format!("{flags}{tag} OK flags\r\n");
        write.write_all(response.as_bytes()).await.unwrap();

        let tag = scripted_expect_command(&mut read, &mut transcript, "SELECT \"INBOX\"").await;
        let response = format!(
            "* {} EXISTS\r\n* OK [UIDVALIDITY {uid_validity}] UIDs valid\r\n{tag} OK selected\r\n",
            remote_uids.len()
        );
        write.write_all(response.as_bytes()).await.unwrap();

        let metadata_fields = "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)]";
        let tag = scripted_expect_command(
            &mut read,
            &mut transcript,
            &format!("UID FETCH {fetched_uid} ({metadata_fields})"),
        )
        .await;
        let headers = format!(
            "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: {subject}\r\nMessage-ID: <{fetched_uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
        );
        let response = format!(
            "* 1 FETCH (UID {fetched_uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" RFC822.SIZE 5 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1) BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
            headers.len()
        );
        write.write_all(response.as_bytes()).await.unwrap();
        write.write_all(headers.as_bytes()).await.unwrap();
        let response = format!(")\r\n{tag} OK metadata\r\n");
        write.write_all(response.as_bytes()).await.unwrap();

        let tag = scripted_expect_command(
            &mut read,
            &mut transcript,
            &format!("UID FETCH {fetched_uid} (BODY.PEEK[]<0.8192>)"),
        )
        .await;
        let response = format!(
            "* 1 FETCH (UID {fetched_uid} BODY[]<0> {{5}}\r\nhello)\r\n{tag} OK preview\r\n"
        );
        write.write_all(response.as_bytes()).await.unwrap();

        let _ = scripted_expect_command(&mut read, &mut transcript, "LOGOUT").await;
        transcript
    }

    #[tokio::test]
    async fn scripted_mail_service_inbox_sync_persists_incremental_uid_and_isolates_accounts() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("scripted-mail-service.sqlite");
        let store = Store::open(&database).await.unwrap();
        let service = MailService::new(store.clone());
        let account = test_account();
        let mut other_account = test_account();
        other_account.email = "other@example.test".into();
        other_account.account_name = other_account.email.clone();
        store.save_account(&account).await.unwrap();
        store.save_account(&other_account).await.unwrap();
        service
            .credentials()
            .set_password(&account, "sync secret")
            .await
            .unwrap();

        let (mut first_client, first_server) = plain_imap_client_and_server().await;
        let first_server = tokio::spawn(scripted_inbox_sync_server(
            first_server,
            77,
            &[42],
            42,
            "Initial transcript message",
        ));
        let first = service
            .sync_mailboxes_with_progress_on_client(
                &mut first_client,
                &account,
                50,
                vec![MailboxPlan::new("INBOX", "INBOX")],
                false,
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(first.synced_count, 1);
        assert!(
            first.new_messages.is_empty(),
            "initial catalogue is not new mail"
        );
        assert_eq!(
            first_server.await.unwrap(),
            vec![
                "LOGIN \"reader@example.test\" \"sync secret\"",
                "LIST \"\" \"*\"",
                "SELECT \"INBOX\"",
                "UID SEARCH ALL",
                "UID FETCH 1:* (UID FLAGS)",
                "SELECT \"INBOX\"",
                "UID FETCH 42 (FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)])",
                "UID FETCH 42 (BODY.PEEK[]<0.8192>)",
                "LOGOUT",
            ]
        );

        let (mut second_client, second_server) = plain_imap_client_and_server().await;
        let second_server = tokio::spawn(scripted_inbox_sync_server(
            second_server,
            77,
            &[42, 43],
            43,
            "Incremental transcript message",
        ));
        let second = service
            .sync_mailboxes_with_progress_on_client(
                &mut second_client,
                &account,
                50,
                vec![MailboxPlan::new("INBOX", "INBOX")],
                false,
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(second.synced_count, 1);
        assert_eq!(
            second
                .new_messages
                .iter()
                .map(|message| message.uid)
                .collect::<Vec<_>>(),
            vec![43]
        );

        assert_eq!(
            store.mailbox_uids(account.id, "INBOX").await.unwrap(),
            [42, 43].into()
        );
        assert!(store
            .mailbox_uids(other_account.id, "INBOX")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .message_by_locator(account.id, "INBOX", 43)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Incremental transcript message"
        );
        let state = store
            .mailbox_catalog_state(account.id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.uid_validity, 77);
        assert_eq!(state.remote_total, 2);
        assert!(state.historical_complete);
        assert_eq!(
            store
                .highest_mailbox_uid(account.id, "INBOX")
                .await
                .unwrap(),
            Some(43)
        );
        assert_eq!(
            second_server.await.unwrap(),
            vec![
                "LOGIN \"reader@example.test\" \"sync secret\"",
                "LIST \"\" \"*\"",
                "SELECT \"INBOX\"",
                "UID SEARCH ALL",
                "UID FETCH 1:* (UID FLAGS)",
                "SELECT \"INBOX\"",
                "UID FETCH 43 (FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)])",
                "UID FETCH 43 (BODY.PEEK[]<0.8192>)",
                "LOGOUT",
            ]
        );

        drop(service);
        drop(store);
        let reopened = Store::open(&database).await.unwrap();
        assert_eq!(
            reopened.mailbox_uids(account.id, "INBOX").await.unwrap(),
            [42, 43].into()
        );
        assert!(reopened
            .mailbox_uids(other_account.id, "INBOX")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened
                .message_by_locator(account.id, "INBOX", 43)
                .await
                .unwrap()
                .unwrap()
                .subject,
            "Incremental transcript message"
        );
    }

    // Test-only CA plus leaf certificate for 127.0.0.1/localhost. The
    // private key is intentionally public: it exists solely to make the
    // loopback TLS transcripts deterministic while still verifying a test CA.
    const SMTP_TEST_CA_DER_BASE64: &str = "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";
    const SMTP_TEST_CERT_DER_BASE64: &str = "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=";
    const SMTP_TEST_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L";

    fn smtp_test_acceptor() -> TlsAcceptor {
        let certificate = CertificateDer::from(STANDARD.decode(SMTP_TEST_CERT_DER_BASE64).unwrap());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            STANDARD.decode(SMTP_TEST_KEY_DER_BASE64).unwrap(),
        ));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
        TlsAcceptor::from(Arc::new(config))
    }

    fn smtp_test_account(security: Security, port: u16) -> Account {
        let mut account = test_account();
        account.email = "sender@example.test".into();
        account.auth = AccountAuth::Password {
            username: "sender@example.test".into(),
        };
        account.smtp_host = "127.0.0.1".into();
        account.smtp_port = port;
        account.smtp_security = security;
        account
    }

    fn smtp_test_tls_parameters(account: &Account) -> TlsParameters {
        let certificate =
            Certificate::from_der(STANDARD.decode(SMTP_TEST_CA_DER_BASE64).unwrap()).unwrap();
        TlsParameters::builder(account.smtp_host.clone())
            .add_root_certificate(certificate)
            .build()
            .unwrap()
    }

    fn smtp_test_endpoint(account: &Account) -> SmtpEndpoint {
        SmtpEndpoint {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            tls_parameters: smtp_test_tls_parameters(account),
        }
    }

    fn smtp_test_envelope() -> Envelope {
        Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            vec!["recipient@example.test".parse().unwrap()],
        )
        .unwrap()
    }

    async fn smtp_command<S>(connection: &mut BufReader<S>) -> String
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut command = String::new();
        assert_ne!(connection.read_line(&mut command).await.unwrap(), 0);
        command
    }

    async fn smtp_reply<S>(connection: &mut BufReader<S>, response: &str)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        connection
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();
    }

    async fn smtp_expect_ehlo_and_auth<S>(connection: &mut BufReader<S>, reject: bool)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        assert!(smtp_command(connection).await.starts_with("EHLO "));
        smtp_reply(
            connection,
            "250-localhost\r\n250-AUTH PLAIN\r\n250 8BITMIME\r\n",
        )
        .await;
        let auth = smtp_command(connection).await;
        assert_eq!(auth, "AUTH PLAIN AHNlbmRlckBleGFtcGxlLnRlc3QAc2VjcmV0\r\n");
        smtp_reply(
            connection,
            if reject {
                "535 5.7.8 invalid credentials\r\n"
            } else {
                "235 2.7.0 authenticated\r\n"
            },
        )
        .await;
    }

    async fn smtp_expect_envelope_until_data<S>(connection: &mut BufReader<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        assert_eq!(
            smtp_command(connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        smtp_reply(connection, "250 2.1.0 sender accepted\r\n").await;
        assert_eq!(
            smtp_command(connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        smtp_reply(connection, "250 2.1.5 recipient accepted\r\n").await;
        assert_eq!(smtp_command(connection).await, "DATA\r\n");
        smtp_reply(connection, "354 send message\r\n").await;
    }

    async fn smtp_read_message<S>(connection: &mut BufReader<S>) -> String
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut message = String::new();
        loop {
            let line = smtp_command(connection).await;
            if line == ".\r\n" {
                return message;
            }
            message.push_str(&line);
        }
    }

    #[tokio::test]
    async fn scripted_smtp_implicit_tls_rejects_a_recipient_after_authentication() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_expect_ehlo_and_auth(&mut connection, false).await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "MAIL FROM:<sender@example.test>\r\n"
            );
            smtp_reply(&mut connection, "250 2.1.0 sender accepted\r\n").await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "RCPT TO:<recipient@example.test>\r\n"
            );
            smtp_reply(&mut connection, "550 5.1.1 no such recipient\r\n").await;
        });

        let error = send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: RCPT\r\n\r\nhello\r\n",
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("550"), "{error:#}");
        assert!(!error.to_string().contains("may be uncertain"), "{error:#}");
    }

    #[tokio::test]
    async fn scripted_smtp_starttls_rejects_bad_credentials_before_envelope_submission() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::StartTls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut plaintext = BufReader::new(stream);
            smtp_reply(&mut plaintext, "220 localhost ready\r\n").await;
            assert!(smtp_command(&mut plaintext).await.starts_with("EHLO "));
            smtp_reply(
                &mut plaintext,
                "250-localhost\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n",
            )
            .await;
            assert_eq!(smtp_command(&mut plaintext).await, "STARTTLS\r\n");
            smtp_reply(&mut plaintext, "220 2.0.0 start TLS\r\n").await;

            let stream = acceptor.accept(plaintext.into_inner()).await.unwrap();
            let mut encrypted = BufReader::new(stream);
            smtp_expect_ehlo_and_auth(&mut encrypted, true).await;
        });

        let error = send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: auth\r\n\r\nhello\r\n",
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("535"), "{error:#}");
        assert!(!error.to_string().contains("may be uncertain"), "{error:#}");
    }

    #[tokio::test]
    async fn scripted_smtp_starttls_eof_after_data_reports_uncertain_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::StartTls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut plaintext = BufReader::new(stream);
            smtp_reply(&mut plaintext, "220 localhost ready\r\n").await;
            assert!(smtp_command(&mut plaintext).await.starts_with("EHLO "));
            smtp_reply(
                &mut plaintext,
                "250-localhost\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n",
            )
            .await;
            assert_eq!(smtp_command(&mut plaintext).await, "STARTTLS\r\n");
            smtp_reply(&mut plaintext, "220 2.0.0 start TLS\r\n").await;

            let stream = acceptor.accept(plaintext.into_inner()).await.unwrap();
            let mut encrypted = BufReader::new(stream);
            smtp_expect_ehlo_and_auth(&mut encrypted, false).await;
            smtp_expect_envelope_until_data(&mut encrypted).await;
            let message = smtp_read_message(&mut encrypted).await;
            assert!(message.contains("Subject: uncertain delivery"));
            // The terminating dot has been consumed, but the server closes
            // before its final 250. The client cannot know whether the relay
            // accepted, queued, or lost the submission.
        });

        let error = send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: uncertain delivery\r\n\r\nhello\r\n",
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(
            error.to_string().contains("delivery may be uncertain"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn scripted_mail_service_send_builds_message_and_skips_duplicate_gmail_append() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let transport_account =
            smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_expect_ehlo_and_auth(&mut connection, false).await;
            smtp_expect_envelope_until_data(&mut connection).await;
            let message = smtp_read_message(&mut connection).await;
            assert!(message.contains("Subject: MailService transcript"));
            assert!(message.contains("plain body"));
            assert!(message.contains("HTML body"));
            smtp_reply(&mut connection, "250 2.0.0 queued as fixture-42\r\n").await;
        });

        let service = MailService::new(Store::in_memory().await.unwrap());
        let endpoint = smtp_test_endpoint(&transport_account);
        let mut gmail = transport_account;
        // The exact production Gmail submission host is the provider contract
        // that prevents a second IMAP APPEND after SMTP accepts the message.
        gmail.smtp_host = "smtp.gmail.com".into();
        let draft = ComposeMessage {
            account_id: gmail.id,
            to: vec!["recipient@example.test".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "MailService transcript".into(),
            body_text: "plain body".into(),
            body_html: Some("<p>HTML body</p>".into()),
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
        };
        let response = service
            .send_with_smtp_endpoint(&gmail, &draft, "secret", endpoint, Duration::from_secs(1))
            .await
            .unwrap();
        server.await.unwrap();
        assert!(response.contains("queued as fixture-42"));
    }

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
            "alternative-mixed-asic-footer" => {
                include_bytes!("../testdata/mime/alternative-mixed-asic-footer.eml")
            }
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

    #[derive(Clone)]
    struct FixtureImapSection {
        headers: Vec<u8>,
        body: Vec<u8>,
    }

    /// The conformance server derives its IMAP view from the exact checked-in
    /// RFC822 bytes.  It deliberately does not use decoded parser contents:
    /// selective fetches must receive the same transfer-encoded leaf bytes a
    /// real server would return for BODY[section].
    struct FixtureImapMessage {
        bodystructure: String,
        root_headers: Vec<u8>,
        sections: BTreeMap<String, FixtureImapSection>,
    }

    fn fixture_imap_message(raw: &[u8]) -> Result<FixtureImapMessage> {
        let parsed = parse_complete_message(raw)?;
        let root = parsed.part(0).context("fixture parser omitted root part")?;
        let mut sections = BTreeMap::new();
        let bodystructure = fixture_bodystructure_part(&parsed, 0, raw, &[], &mut sections)?;
        let root_headers = raw
            .get(root.raw_header_offset() as usize..root.raw_body_offset() as usize)
            .context("fixture root header offsets are invalid")?
            .to_vec();
        Ok(FixtureImapMessage {
            bodystructure,
            root_headers,
            sections,
        })
    }

    fn fixture_bodystructure_part(
        message: &ParsedMessage<'_>,
        part_id: u32,
        raw: &[u8],
        path: &[usize],
        sections: &mut BTreeMap<String, FixtureImapSection>,
    ) -> Result<String> {
        let part = message
            .part(part_id)
            .context("fixture parser omitted MIME part")?;
        let section = FixtureImapSection {
            headers: raw
                .get(part.raw_header_offset() as usize..part.raw_body_offset() as usize)
                .context("fixture MIME header offsets are invalid")?
                .to_vec(),
            body: raw
                .get(part.raw_body_offset() as usize..part.raw_end_offset() as usize)
                .context("fixture MIME body offsets are invalid")?
                .to_vec(),
        };
        sections.insert(section_name(path), section);

        let content_type = part
            .content_type()
            .context("fixture MIME part omitted Content-Type")?;
        if let Some(children) = part.sub_parts() {
            let mut values = Vec::with_capacity(children.len() + 5);
            for (index, child) in children.iter().enumerate() {
                let mut child_path = path.to_vec();
                child_path.push(index + 1);
                values.push(fixture_bodystructure_part(
                    message,
                    *child,
                    raw,
                    &child_path,
                    sections,
                )?);
            }
            values.push(imap_fixture_string(
                content_type.subtype().unwrap_or("mixed"),
            ));
            values.push(imap_fixture_params(content_type.attributes()));
            values.push(imap_fixture_disposition(part));
            values.push("NIL".into());
            values.push("NIL".into());
            return Ok(format!("({})", values.join(" ")));
        }

        let raw_size = part.raw_end_offset().saturating_sub(part.raw_body_offset()) as usize;
        Ok(format!(
            "({} {} {} {} NIL {} {} 0 {} NIL NIL)",
            imap_fixture_string(content_type.ctype()),
            imap_fixture_string(content_type.subtype().unwrap_or("plain")),
            imap_fixture_params(content_type.attributes()),
            part.content_id()
                .map(imap_fixture_string)
                .unwrap_or_else(|| "NIL".into()),
            imap_fixture_string(part.content_transfer_encoding().unwrap_or("7BIT")),
            raw_size,
            imap_fixture_disposition(part),
        ))
    }

    fn imap_fixture_params(attributes: Option<&[mail_parser::Attribute<'_>]>) -> String {
        let Some(attributes) = attributes.filter(|attributes| !attributes.is_empty()) else {
            return "NIL".into();
        };
        let mut values = Vec::with_capacity(attributes.len() * 2);
        for attribute in attributes {
            values.push(imap_fixture_string(&attribute.name));
            values.push(imap_fixture_string(&attribute.value));
        }
        format!("({})", values.join(" "))
    }

    fn imap_fixture_disposition(part: &MessagePart<'_>) -> String {
        let Some(disposition) = part.content_disposition() else {
            return "NIL".into();
        };
        format!(
            "({} {})",
            imap_fixture_string(disposition.ctype()),
            imap_fixture_params(disposition.attributes())
        )
    }

    fn imap_fixture_string(value: &str) -> String {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace(['\r', '\n'], " ")
        )
    }

    async fn serve_fixture_selective_sections(
        stream: DuplexStream,
        uid: u32,
        sections: BTreeMap<String, FixtureImapSection>,
    ) -> Result<BTreeSet<String>> {
        let (read, mut write) = split(stream);
        let mut reader = BufReader::new(read);
        let mut served = BTreeSet::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(served);
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .context("fixture server received an untagged command")?;
            let section = command
                .split_once("BODY.PEEK[")
                .and_then(|(_, rest)| rest.split_once(']'))
                .map(|(section, _)| section)
                .context("fixture server received a non-sectioned fetch")?;
            let (path, mime_headers) = section
                .strip_suffix(".MIME")
                .map(|path| (path, true))
                .unwrap_or((section, false));
            let key = if path.eq_ignore_ascii_case("TEXT") {
                ""
            } else {
                path
            };
            let item = sections
                .get(key)
                .with_context(|| format!("fixture server lacks BODY[{section}]"))?;
            let bytes = if mime_headers {
                &item.headers
            } else {
                &item.body
            };
            served.insert(section.to_owned());
            write
                .write_all(
                    format!(
                        "* 1 FETCH (UID {uid} BODY[{section}] {{{}}}\r\n",
                        bytes.len()
                    )
                    .as_bytes(),
                )
                .await?;
            write.write_all(bytes).await?;
            write
                .write_all(format!("\r\n)\r\n{tag} OK FETCH completed\r\n").as_bytes())
                .await?;
            write.flush().await?;
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SelectiveUserVisibleSemantics {
        body_text: String,
        body_html: Option<String>,
        snippet: String,
        attachments: Vec<(String, String, bool, String)>,
    }

    fn selective_user_visible_semantics(message: &MailSummary) -> SelectiveUserVisibleSemantics {
        let mut attachments = message
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.attachment.filename.clone(),
                    attachment.attachment.mime_type.clone(),
                    attachment.attachment.is_inline,
                    format!("{:?}", attachment.attachment.presentation),
                )
            })
            .collect::<Vec<_>>();
        attachments.sort();
        SelectiveUserVisibleSemantics {
            body_text: message.body_text.clone(),
            body_html: message.body_html.clone(),
            snippet: message.snippet.clone(),
            attachments,
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

        let alternative_mixed = parse_message(
            &account,
            "INBOX",
            2,
            &[],
            mime_corpus("alternative-mixed-asic-footer"),
            None,
        )
        .await
        .unwrap();
        let alternative_html = alternative_mixed.body_html.as_deref().unwrap();
        assert!(alternative_html.contains("Redacted secure delivery."));
        assert_eq!(
            alternative_html
                .matches("Manage delivery preferences")
                .count(),
            2
        );
        assert!(alternative_mixed.body_text.contains("Plain fallback"));
        assert_eq!(alternative_mixed.attachments.len(), 1);
        assert_eq!(
            alternative_mixed.attachments[0].attachment.filename,
            "redacted-delivery.asice"
        );
        assert_eq!(
            alternative_mixed.attachments[0].attachment.mime_type,
            "application/vnd.etsi.asic-e+zip"
        );

        let nested = parse_message(
            &account,
            "INBOX",
            3,
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
            4,
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
            5,
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
            6,
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
            7,
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
            8,
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
            9,
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
            10,
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
            11,
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
            ("alternative-mixed-asic-footer", true),
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
    async fn realistic_selective_bodystructure_fixtures_match_complete_parser_and_starred_storage()
    {
        // Keep this list in lockstep with `selective-bodystructure` in the
        // governed manifest. Each fixture is served as transfer-encoded IMAP
        // sections generated from its actual raw RFC822 representation.
        let cases = [
            "attached-message-rfc822",
            "format-flowed-delsp",
            "linkedin-inline-content-id",
            "multipart-attachment-container",
            "provider-signature-inline",
        ];
        let manifest: serde_json::Value = serde_json::from_str(include_str!(
            "../../../testdata/realistic-fixtures.manifest.json"
        ))
        .unwrap();
        let mut declared = manifest["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fixture| {
                fixture["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with(".eml"))
                    && fixture["applicablePaths"].as_array().is_some_and(|paths| {
                        paths
                            .iter()
                            .any(|path| path.as_str() == Some("selective-bodystructure"))
                    })
            })
            .map(|fixture| {
                fixture["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("mime.")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        declared.sort();
        assert_eq!(
            declared, cases,
            "manifest selective fixtures must all be covered"
        );
        let account = test_account();
        let response_lines = vec![
            "* 1 FETCH (UID 1 FLAGS (\\Seen \\Flagged) INTERNALDATE \"21-Jul-2026 10:00:00 +0000\")"
                .to_owned(),
        ];
        let mut complete_messages = Vec::new();

        for (index, name) in cases.iter().enumerate() {
            let raw = if *name == "provider-signature-inline" {
                include_bytes!("../tests/fixtures/provider-signature-inline.eml").as_slice()
            } else {
                mime_corpus(name)
            };
            let uid = index as u32 + 1;
            let complete = parse_message(&account, "INBOX", uid, &response_lines, raw, None)
                .await
                .unwrap_or_else(|error| panic!("{name} complete parse failed: {error}"));
            let fixture = fixture_imap_message(raw)
                .unwrap_or_else(|error| panic!("{name} fixture IMAP derivation failed: {error}"));
            let structure = parse_bodystructure(&[format!(
                "* 1 FETCH (UID {uid} BODYSTRUCTURE {})",
                fixture.bodystructure
            )])
            .unwrap_or_else(|error| {
                panic!("{name} generated BODYSTRUCTURE failed to parse: {error}")
            });
            let (client_transport, server_transport) =
                duplex(MAX_RAW_MESSAGE_BYTES.min(1024 * 1024));
            let server = tokio::spawn(serve_fixture_selective_sections(
                server_transport,
                uid,
                fixture.sections,
            ));
            let mut client = ImapClient {
                reader: BufReader::new(client_transport),
                tag: 0,
            };
            let selective = fetch_selective_message(
                &mut client,
                &account,
                "INBOX",
                uid,
                &response_lines,
                &fixture.root_headers,
                &structure,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} selective fetch failed: {error}"));
            drop(client);
            let served = server
                .await
                .unwrap_or_else(|error| panic!("{name} fixture server panicked: {error}"))
                .unwrap_or_else(|error| panic!("{name} fixture server failed: {error}"));

            assert!(
                served
                    .iter()
                    .all(|section| { section.as_bytes().first().is_some_and(u8::is_ascii_digit) }),
                "{name} selective fetch must use only nested BODY sections: {served:?}"
            );
            assert_eq!(
                selective_user_visible_semantics(&selective),
                selective_user_visible_semantics(&complete),
                "{name} complete and selective user-visible content diverged"
            );
            complete_messages.push(complete);
        }

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("selective-fixtures.sqlite");
        let store = Store::open(&database).await.unwrap();
        store.save_account(&account).await.unwrap();
        store.upsert_messages(&complete_messages).await.unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        for complete in &complete_messages {
            let stored = reopened.message(&complete.id).await.unwrap().unwrap();
            assert_eq!(stored.body_text, complete.body_text, "{}", complete.id);
            assert_eq!(stored.body_html, complete.body_html, "{}", complete.id);
            assert_eq!(stored.snippet, complete.snippet, "{}", complete.id);
            let metadata = reopened
                .starred_attachment_metadata(&complete.id)
                .await
                .unwrap();
            assert_eq!(
                metadata.len(),
                complete
                    .attachments
                    .iter()
                    .filter(|attachment| attachment.attachment.presentation.is_downloadable())
                    .count(),
                "{}",
                complete.id
            );
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
    async fn complete_parser_attachment_ids_match_selective_mime_section_ids() {
        let account = test_account();
        let raw = concat!(
            "Date: Tue, 21 Jul 2026 10:00:00 +0000\r\n",
            "From: sender@example.test\r\nTo: recipient@example.test\r\n",
            "Subject: Section identity\r\nMIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=m\r\n\r\n",
            "--m\r\nContent-Type: text/plain\r\n\r\nBody\r\n",
            "--m\r\nContent-Type: application/pdf; name=receipt.pdf\r\n",
            "Content-Disposition: attachment; filename=receipt.pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\ncGRm\r\n--m--\r\n"
        );
        let message = parse_message(&account, "INBOX", 9, &[], raw.as_bytes(), None)
            .await
            .unwrap();
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.attachments[0].attachment.id,
            format!("{}:mime-v1:2", message.id)
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

    #[test]
    fn inline_resource_detection_and_rewriting_ignore_visible_cid_text() {
        let html = "<p>\"cid:logo\" remains visible</p><img src=\"cid:logo\">";
        assert!(html_contains_reference(html, "cid:logo"));
        assert!(!html_contains_reference(
            "<p>\"cid:logo\" remains visible</p>",
            "cid:logo"
        ));
        let resolved =
            replace_resource_reference(html, "cid:logo", "data:image/png;base64,AA").unwrap();
        assert!(resolved.contains("<p>\"cid:logo\" remains visible</p>"));
        assert!(resolved.contains("src=\"data:image/png;base64,AA\""));
    }

    #[test]
    fn inline_resource_rewriting_accepts_spaced_unquoted_and_quote_aware_attributes() {
        let html = concat!(
            "<p>cid:logo remains visible text</p>",
            "<img alt=\"comparison: 2 > 1; url(cid:style)\" SRC = cid:logo>",
            "<a href = 'cid:other'>link</a><table style=\"background: url( 'cid:style' )\">"
        );
        assert!(html_contains_reference(html, "CID:LOGO"));
        assert!(html_contains_reference(html, "cid:other"));
        assert!(html_contains_reference(html, "cid:style"));
        assert!(!html_contains_reference(html, "cid:visible"));
        let resolved = rewrite_html_resource_references(
            html,
            &BTreeMap::from([
                ("cid:logo".into(), "data:image/png;base64,AA".into()),
                ("cid:other".into(), "data:image/png;base64,BB".into()),
                ("cid:style".into(), "data:image/png;base64,CC".into()),
            ]),
        )
        .unwrap();
        assert!(resolved.contains("cid:logo remains visible text"));
        assert!(resolved.contains("comparison: 2 > 1; url(cid:style)"));
        assert!(resolved.contains("SRC = data:image/png;base64,AA"));
        assert!(resolved.contains("href = 'data:image/png;base64,BB'"));
        assert!(resolved.contains("url( 'data:image/png;base64,CC' )"));
    }

    #[test]
    fn html_joining_never_discards_actionable_sibling_markup() {
        let merged = join_visible_html_segments(vec![
            "<p>Primary delivery</p><p>Manage preferences</p>".into(),
            "<a href=\"https://example.test/manage\">Manage preferences</a><img src=\"cid:seal\">"
                .into(),
        ])
        .unwrap();
        assert_eq!(merged.matches("Manage preferences").count(), 2);
        assert!(merged.contains("https://example.test/manage"));
        assert!(merged.contains("cid:seal"));
    }

    #[test]
    fn inline_resource_detection_and_rewriting_ignore_html_comments() {
        let html = concat!(
            "<!--[if mso]><img src=\"cid:comment-only\"><![endif]-->",
            "<script>const template = '</scripture><img src=\"cid:script-only\">';</script>",
            "<iframe><img src=\"cid:iframe-only\"></iframe>",
            "<img src=\"cid:live\">"
        );
        let references = html_resource_references(html);
        assert!(!references.contains("cid:comment-only"));
        assert!(!references.contains("cid:script-only"));
        assert!(!references.contains("cid:iframe-only"));
        assert!(references.contains("cid:live"));

        let resolved = rewrite_html_resource_references(
            html,
            &BTreeMap::from([
                ("cid:comment-only".into(), "data:image/png;base64,AA".into()),
                ("cid:script-only".into(), "data:image/png;base64,CC".into()),
                ("cid:iframe-only".into(), "data:image/png;base64,DD".into()),
                ("cid:live".into(), "data:image/png;base64,BB".into()),
            ]),
        )
        .unwrap();
        assert!(resolved.contains("src=\"cid:comment-only\""));
        assert!(resolved.contains("src=\"cid:script-only\""));
        assert!(resolved.contains("src=\"cid:iframe-only\""));
        assert!(resolved.contains("src=\"data:image/png;base64,BB\""));
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
            assert_eq!(
                message.to_addresses,
                "Primary <primary@example.test>, Team: second@example.test, Third <third@example.test>;"
            );
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
    fn full_foreground_fetches_are_read_neutral_for_gmail_and_generic_imap() {
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
    fn selective_fetch_commands_are_sectioned_read_neutral_and_never_request_pdf_bodies() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (((\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 450 10) ",
            "(\"IMAGE\" \"PNG\" (\"NAME\" \"image001.png\") \"<image001.png@redacted>\" NIL \"BASE64\" 8 NIL (\"INLINE\" (\"FILENAME\" \"image001.png\"))) \"RELATED\" (\"BOUNDARY\" \"related-redacted\")) ",
            "(\"APPLICATION\" \"PDF\" (\"NAME\" \"claim-documents.pdf\") NIL NIL \"BASE64\" 4 NIL (\"ATTACHMENT\" (\"FILENAME\" \"claim-documents.pdf\"))) \"MIXED\" (\"BOUNDARY\" \"mixed-redacted\")))"
        ).into()]).unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert_eq!(
            plan.text_parts
                .iter()
                .map(|part| section_name(&part.part.path))
                .collect::<Vec<_>>(),
            vec!["1.1"]
        );
        assert_eq!(
            plan.attachments
                .iter()
                .map(|part| section_name(&part.part.path))
                .collect::<Vec<_>>(),
            vec!["1.2", "2"]
        );
        let commands = plan
            .text_parts
            .iter()
            .flat_map(|part| {
                [
                    section_mime_fetch_command(2965, &part.part.path),
                    section_fetch_command(2965, &part.part.path),
                ]
            })
            .collect::<Vec<_>>();
        assert!(commands
            .iter()
            .all(|command| command.contains("BODY.PEEK[1.1")));
        assert!(commands
            .iter()
            .all(|command| !command.contains("BODY.PEEK[]")
                && !command.contains("RFC822")
                && !command.contains("BODY[")));
        assert!(commands.iter().all(|command| !command.contains("[2]")));
    }

    #[test]
    fn top_level_text_reuses_root_headers_and_never_fetches_a_mime_section() {
        assert_eq!(
            section_fetch_command(42, &[]),
            "UID FETCH 42 (BODY.PEEK[TEXT])"
        );
        assert_eq!(nested_section_mime_fetch_command(42, &[]), None);
        assert_eq!(
            nested_section_mime_fetch_command(42, &[1]),
            Some("UID FETCH 42 (BODY.PEEK[1.MIME])".into())
        );
    }

    #[test]
    fn recent_refresh_limits_main_folder_selection_and_formats_imap_since_dates() {
        let mut gmail = test_account();
        gmail.provider_id = "gmail".into();
        gmail.archive_mailbox = "[Gmail]/All Mail".into();
        let plans = refresh_main_mailbox_plans(mailbox_plans(&gmail));
        assert_eq!(
            plans.iter().map(|plan| plan.local).collect::<Vec<_>>(),
            vec!["INBOX", "Sent", "Archive"]
        );
        assert_eq!(plans[1].remote, "[Gmail]/Sent Mail");
        assert_eq!(plans[2].remote, "[Gmail]/All Mail");
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-07-15T23:59:59+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let since = imap_since_date(cutoff);
        assert_eq!(since, "15-Jul-2026");
        assert_eq!(
            recent_uid_search_command(&since),
            "UID SEARCH SINCE 15-Jul-2026"
        );
    }

    #[test]
    fn recent_refresh_is_account_bounded_and_newest_first_with_fair_folder_allocation() {
        let state = crate::storage::MailboxCatalogState {
            account_id: "account".into(),
            mailbox: "INBOX".into(),
            remote_name: "INBOX".into(),
            uid_validity: 1,
            remote_total: 4,
            historical_complete: false,
        };
        let mut work = vec![
            RecentRefreshWork {
                plan: MailboxPlan::new("INBOX", "INBOX"),
                state: state.clone(),
                uid_validity: 1,
                uids: recent_uids_newest_first(vec![4, 9, 7]),
                selected_uids: Vec::new(),
            },
            RecentRefreshWork {
                plan: MailboxPlan::new("Sent", "Sent"),
                state: state.clone(),
                uid_validity: 1,
                uids: recent_uids_newest_first(vec![2, 8]),
                selected_uids: Vec::new(),
            },
            RecentRefreshWork {
                plan: MailboxPlan::new("Archive", "Archive"),
                state,
                uid_validity: 1,
                uids: recent_uids_newest_first(vec![6]),
                selected_uids: Vec::new(),
            },
        ];
        allocate_recent_refresh_uids(&mut work, 4);

        assert_eq!(
            work.iter()
                .map(|item| item.selected_uids.len())
                .sum::<usize>(),
            4
        );
        assert_eq!(work[0].selected_uids, vec![9, 7]);
        assert_eq!(work[1].selected_uids, vec![8]);
        assert_eq!(work[2].selected_uids, vec![6]);
    }

    #[test]
    fn recent_refresh_fetches_only_catalogue_headers_and_sectioned_snippets() {
        let account = test_account();
        let fields = catalogue_fetch_fields(&account);
        let snippet = section_partial_fetch_command(42, &[1, 1], 8192);

        assert!(fields.contains("BODYSTRUCTURE"));
        assert!(fields.contains("BODY.PEEK[HEADER.FIELDS"));
        assert!(fields.contains("RFC822.SIZE"));
        assert!(!fields.contains("BODY.PEEK[]"));
        assert!(!fields.contains(" RFC822)"));
        assert_eq!(snippet, "UID FETCH 42 (BODY.PEEK[1.1]<0.8192>)");
        assert!(!snippet.contains("BODY.PEEK[]"));
    }

    #[test]
    fn selective_bodystructure_parser_keeps_provider_signature_html_and_pdf_metadata_separate() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (((\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 450 10) ",
            "(\"IMAGE\" \"PNG\" (\"NAME\" \"image001.png\") \"<image001.png@redacted>\" NIL \"BASE64\" 8 NIL (\"INLINE\" (\"FILENAME\" \"image001.png\"))) \"RELATED\") ",
            "(\"APPLICATION\" \"PDF\" (\"NAME\" \"claim-documents.pdf\") NIL NIL \"BASE64\" 4 NIL (\"ATTACHMENT\" (\"FILENAME\" \"claim-documents.pdf\"))) \"MIXED\"))"
        ).into()]).unwrap();
        let plan = selective_plan(&structure).unwrap();
        let id = "message-2965";
        let image = attachment_data_from_part(&plan.attachments[0], id, b"Content-Type: image/png\r\nContent-ID: <image001.png@redacted>\r\nContent-Disposition: inline; filename=image001.png\r\n", None, true).unwrap();
        let pdf = attachment_data_from_part(&plan.attachments[1], id, &[], None, false).unwrap();

        assert!(image.bytes.is_empty());
        assert_eq!(
            image.attachment.presentation,
            AttachmentPresentation::Embedded
        );
        assert!(pdf.bytes.is_empty());
        assert_eq!(pdf.attachment.filename, "claim-documents.pdf");
        assert_eq!(pdf.attachment.id, "message-2965:mime-v1:2");
        assert_eq!(
            pdf.attachment.presentation,
            AttachmentPresentation::Downloadable
        );
    }

    #[test]
    fn selective_attachment_only_messages_keep_metadata_and_targeted_pdf_bytes() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (\"APPLICATION\" \"PDF\" (\"NAME\" \"claim-documents.pdf\") NIL NIL \"BASE64\" 4 NIL ",
            "(\"ATTACHMENT\" (\"FILENAME\" \"claim-documents.pdf\"))))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();
        let data = attachment_data_from_part(
            &plan.attachments[0],
            "message-2965",
            b"Content-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=claim-documents.pdf\r\n",
            Some(b"cGRm".to_vec()),
            false,
        )
        .unwrap();

        assert!(plan.text_parts.is_empty());
        assert_eq!(plan.attachments.len(), 1);
        assert_eq!(data.attachment.id, "message-2965:mime-v1:root");
        assert_eq!(data.attachment.filename, "claim-documents.pdf");
        assert_eq!(data.bytes, b"pdf");
    }

    #[test]
    fn selective_image_only_messages_do_not_require_a_text_part() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (\"IMAGE\" \"PNG\" (\"NAME\" \"logo.png\") \"<logo@example>\" NIL \"BASE64\" 8 NIL ",
            "(\"INLINE\" (\"FILENAME\" \"logo.png\"))))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();
        let data =
            attachment_data_from_part(&plan.attachments[0], "message-1", &[], None, false).unwrap();

        assert!(plan.text_parts.is_empty());
        assert_eq!(data.attachment.filename, "logo.png");
        assert_eq!(
            data.attachment.presentation,
            AttachmentPresentation::Downloadable
        );
    }

    #[test]
    fn selective_empty_text_filename_remains_a_body_candidate() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"HTML\" (\"NAME\" \"\") \"<body@example>\" NIL ",
            "\"7BIT\" 12 1 NIL (\"INLINE\" (\"FILENAME\" \"\"))))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert_eq!(plan.text_parts.len(), 1);
        assert!(plan.attachments.is_empty());
        assert_eq!(plan.text_parts[0].part.mime_type, "text/html");
    }

    #[test]
    fn attached_multipart_branch_is_one_downloadable_candidate_not_outer_body_content() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (((\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 12 1) \"RELATED\" NIL ",
            "(\"ATTACHMENT\" (\"FILENAME\" \"forwarded.eml\"))) \"MIXED\"))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert!(plan.text_parts.is_empty());
        assert_eq!(plan.attachments.len(), 1);
        assert_eq!(plan.attachments[0].part.path, vec![1]);
        assert_eq!(plan.attachments[0].part.filename(), Some("forwarded.eml"));
    }

    #[test]
    fn attached_message_rfc822_keeps_its_outer_disposition_instead_of_descending_into_it() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (\"MESSAGE\" \"RFC822\" (\"NAME\" \"forwarded.eml\") NIL NIL \"7BIT\" 123 ",
            "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 5 1) 1 NIL ",
            "(\"ATTACHMENT\" (\"FILENAME\" \"forwarded.eml\"))))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert!(plan.text_parts.is_empty());
        assert_eq!(plan.attachments.len(), 1);
        assert!(plan.attachments[0].part.is_explicit_attachment());
        assert_eq!(plan.attachments[0].part.filename(), Some("forwarded.eml"));
    }

    #[test]
    fn duplicate_inline_references_are_ambiguous_and_remain_downloadable() {
        let html = "<img src=\"cid:duplicate@example\">";
        let references = vec!["cid:duplicate@example".to_owned()];
        let counts = BTreeMap::from([("cid:duplicate@example".to_owned(), 2usize)]);

        assert!(!has_unambiguous_html_reference(
            &html_resource_references(html),
            &references,
            &counts
        ));
    }

    #[test]
    fn selective_part_decoding_preserves_visible_html_cid_data_and_remote_urls() {
        // The checked-in provider-shaped fixture is the real regression shape:
        // multipart/mixed -> related, quoted-printable HTML, CID logo, PDF.
        let fixture = include_str!("../tests/fixtures/provider-signature-inline.eml");
        let parsed = parse_complete_message(fixture.as_bytes()).unwrap();
        let html = format!(
            "{}<img src=\"https://images.example.test/logo.png\">",
            select_bodies(&parsed).unwrap().html.unwrap()
        );
        let resolved = resolve_inline_images_from_raw(html, &parsed, fixture.as_bytes()).unwrap();

        assert!(resolved.contains("Redacted message content"));
        assert!(resolved.contains("data:image/png;base64,iVBORw0KGgo="));
        assert!(resolved.contains("https://images.example.test/logo.png"));
    }

    #[test]
    fn selective_text_decoding_uses_hardened_charset_transfer_and_flowed_rules() {
        assert_eq!(
            decode_mime_part_body(
                b"Content-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n",
                b"caf=E9",
            )
            .unwrap(),
            "café"
        );
        assert_eq!(
            decode_mime_part_body(
                b"Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n",
                b"PHA+aGVsbG88L3A+",
            )
            .unwrap(),
            "<p>hello</p>"
        );
        assert_eq!(
            decode_mime_part_body(
                b"Content-Type: text/plain; charset=utf-8; format=flowed; delsp=yes\r\nContent-Transfer-Encoding: 7bit\r\n",
                b"flowed \r\nline",
            )
            .unwrap(),
            "flowedline"
        );
        assert!(decode_mime_part_body(
            b"Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: x-broken\r\n",
            b"<p>ignored</p>",
        )
        .is_err());
    }

    #[test]
    fn selective_alternative_keeps_plain_fallback_when_html_is_undecodable() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 5 1) ",
            "(\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 8 1) ",
            "(\"TEXT\" \"HTML\" NIL NIL NIL \"X-BROKEN\" 9 1) \"ALTERNATIVE\"))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert_eq!(
            plan.text_parts
                .iter()
                .map(|part| (part.part.mime_type.as_str(), part.part.path.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                // The last alternative is attempted first, but a broken
                // transfer encoding falls back to the previous HTML body.
                ("text/html", [3].as_slice()),
                ("text/html", [2].as_slice()),
                ("text/plain", [1].as_slice()),
            ]
        );
        let mut successful = BTreeMap::new();
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[0],
            &successful
        ));
        // The newest HTML branch is broken, so it cannot claim the group.
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[1],
            &successful
        ));
        record_successful_alternative_branches(&plan.text_parts[1], &mut successful);
        // HTML and plain representations are selected independently, so a
        // valid HTML branch does not erase the useful plain fallback.
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[2],
            &successful
        ));
    }

    #[test]
    fn selective_alternative_keeps_all_mixed_html_siblings_in_its_selected_branch() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 5 1) ",
            "((\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 30 1) ",
            "(\"APPLICATION\" \"VND.ETSI.ASIC-E+ZIP\" (\"NAME\" \"redacted-delivery.asice\") NIL NIL \"BASE64\" 40 NIL ",
            "(\"ATTACHMENT\" (\"FILENAME\" \"redacted-delivery.asice\"))) ",
            "(\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 20 1) \"MIXED\") \"ALTERNATIVE\"))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert_eq!(
            plan.text_parts
                .iter()
                .map(|part| part.part.path.as_slice())
                .collect::<Vec<_>>(),
            vec![[2, 1].as_slice(), [2, 3].as_slice(), [1].as_slice()]
        );
        assert_eq!(plan.attachments.len(), 1);
        assert_eq!(plan.attachments[0].part.path, vec![2, 2]);

        let mut successful = BTreeMap::new();
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[0],
            &successful
        ));
        record_successful_alternative_branches(&plan.text_parts[0], &mut successful);
        // 2.3 is a sibling in the successful mixed branch, not a fallback.
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[1],
            &successful
        ));
        record_successful_alternative_branches(&plan.text_parts[1], &mut successful);
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[2],
            &successful
        ));
    }

    #[test]
    fn selective_composite_footer_cannot_claim_branch_after_primary_html_failure() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 25 1) ",
            "((\"TEXT\" \"HTML\" NIL NIL NIL \"X-BROKEN\" 30 1) ",
            "(\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 20 1) \"MIXED\") \"ALTERNATIVE\"))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();
        let mut successful = BTreeMap::new();

        // 2.1 fails and therefore does not select the preferred composite.
        assert!(plan.text_parts[0].alternative_branches[0].decisive);
        // A decoded footer is retained provisionally, but cannot select it.
        assert!(!plan.text_parts[1].alternative_branches[0].decisive);
        record_successful_alternative_branches(&plan.text_parts[1], &mut successful);
        assert!(alternative_branch_is_eligible(
            &plan.text_parts[2],
            &successful
        ));

        record_successful_alternative_branches(&plan.text_parts[2], &mut successful);
        assert!(!alternative_branch_is_selected(
            &plan.text_parts[1],
            &successful
        ));
        assert!(alternative_branch_is_selected(
            &plan.text_parts[2],
            &successful
        ));
    }

    #[test]
    fn selective_nested_alternative_fallback_can_select_its_enclosing_branch() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 25 1) ",
            "((\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 30 1) ",
            "(\"TEXT\" \"HTML\" NIL NIL NIL \"X-BROKEN\" 20 1) \"ALTERNATIVE\") ",
            "\"ALTERNATIVE\"))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();
        let mut successful = BTreeMap::new();

        assert_eq!(plan.text_parts[0].part.path, vec![2, 2]);
        assert_eq!(plan.text_parts[1].part.path, vec![2, 1]);
        assert!(plan.text_parts[1]
            .alternative_branches
            .iter()
            .all(|branch| branch.decisive));
        record_successful_alternative_branches(&plan.text_parts[1], &mut successful);
        assert!(alternative_branch_is_selected(
            &plan.text_parts[1],
            &successful
        ));
        assert!(!alternative_branch_is_selected(
            &plan.text_parts[2],
            &successful
        ));
    }

    #[test]
    fn selective_related_honors_the_declared_start_root() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"HTML\" NIL \"<wrong>\" NIL \"7BIT\" 5 1) ",
            "(\"TEXT\" \"PLAIN\" NIL \"<declared-root>\" NIL \"7BIT\" 4 1) ",
            "\"RELATED\" (\"START\" \"<declared-root>\")))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();

        assert_eq!(plan.text_parts.len(), 1);
        assert_eq!(plan.text_parts[0].part.mime_type, "text/plain");
        assert_eq!(plan.text_parts[0].part.path, vec![2]);
    }

    #[test]
    fn selective_related_without_a_matching_start_uses_its_first_child() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" NIL \"<first>\" NIL \"7BIT\" 5 1) ",
            "(\"TEXT\" \"HTML\" NIL \"<second>\" NIL \"7BIT\" 5 1) ",
            "\"RELATED\" (\"START\" \" <missing> \")))"
        )
        .into()])
        .unwrap();
        let plan = selective_plan(&structure).unwrap();
        assert_eq!(plan.text_parts.len(), 1);
        assert_eq!(plan.text_parts[0].part.path, vec![1]);
        assert_eq!(plan.text_parts[0].part.mime_type, "text/plain");
    }

    #[test]
    fn malformed_related_start_matches_full_parse_first_child_fallback() {
        let raw = concat!(
            "Content-Type: multipart/related; start=\" <missing@example.test> \"; boundary=rel\r\n\r\n",
            "--rel\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>First related root</p>\r\n",
            "--rel\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSecond related sibling\r\n--rel--\r\n"
        );
        let parsed = parse_complete_message(raw.as_bytes()).unwrap();
        let selected = select_bodies(&parsed).unwrap();
        assert!(selected
            .html
            .as_deref()
            .is_some_and(|html| html.contains("First related root")));
        assert!(selected.text.is_empty());
    }

    #[test]
    fn targeted_attachment_allows_transport_overhead_but_enforces_decoded_limit() {
        assert!(validate_targeted_attachment_bytes(&vec![0; MAX_ATTACHMENT_BYTES]).is_ok());
        assert!(validate_targeted_attachment_bytes(&vec![0; MAX_ATTACHMENT_BYTES + 1]).is_err());
    }

    #[test]
    fn selective_bodystructure_decodes_extended_rfc2231_filenames() {
        let structure = parse_bodystructure(&[concat!(
            "* 1 FETCH (BODYSTRUCTURE (\"APPLICATION\" \"PDF\" (\"NAME*\" ",
            "\"utf-8''caf%C3%A9.pdf\") NIL NIL \"BASE64\" 4 NIL ",
            "(\"ATTACHMENT\" (\"FILENAME*\" \"utf-8''caf%C3%A9.pdf\"))))"
        )
        .into()])
        .unwrap();
        assert_eq!(structure.filename(), Some("café.pdf"));
    }

    #[test]
    fn selective_bodystructure_rejects_malformed_and_oversized_parts_before_literal_allocation() {
        assert!(parse_bodystructure(&["* 1 FETCH (BODYSTRUCTURE (\"TEXT\"".into()]).is_err());
        assert!(display_literal_limit(0, MAX_DISPLAY_PART_BYTES + 1, "text").is_err());
        assert!(display_literal_limit(MAX_DISPLAY_TOTAL_BYTES - 10, 11, "text").is_err());
        assert_eq!(
            display_literal_limit(0, 1024, "text").unwrap(),
            MAX_DISPLAY_PART_BYTES
        );
    }

    #[test]
    fn selective_bodystructure_enforces_depth_part_and_attachment_budgets() {
        let text_leaf = || {
            ImapBodyValue::List(vec![
                ImapBodyValue::Atom("TEXT".into()),
                ImapBodyValue::Atom("PLAIN".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("7BIT".into()),
                ImapBodyValue::Atom("1".into()),
            ])
        };
        let multipart = |children: Vec<ImapBodyValue>| {
            let mut values = children;
            values.push(ImapBodyValue::Atom("MIXED".into()));
            values.push(ImapBodyValue::Atom("NIL".into()));
            ImapBodyValue::List(values)
        };

        let mut deeply_nested = text_leaf();
        for _ in 0..=MAX_MULTIPART_NESTING {
            deeply_nested = multipart(vec![deeply_nested]);
        }
        assert!(bodystructure_part(&deeply_nested, Vec::new())
            .unwrap_err()
            .to_string()
            .contains("mime_multipart_nesting_too_deep"));

        let too_many_parts = multipart((0..MAX_MIME_PARTS).map(|_| text_leaf()).collect());
        assert!(bodystructure_part(&too_many_parts, Vec::new())
            .unwrap_err()
            .to_string()
            .contains("mime_too_many_parts"));

        let attachment_leaf = || {
            ImapBodyValue::List(vec![
                ImapBodyValue::Atom("APPLICATION".into()),
                ImapBodyValue::Atom("OCTET-STREAM".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("NIL".into()),
                ImapBodyValue::Atom("BASE64".into()),
                ImapBodyValue::Atom("1".into()),
            ])
        };
        let too_many_attachments = bodystructure_part(
            &multipart(
                (0..=MAX_ATTACHMENT_COUNT)
                    .map(|_| attachment_leaf())
                    .collect(),
            ),
            Vec::new(),
        )
        .unwrap();
        assert!(selective_plan(&too_many_attachments)
            .unwrap_err()
            .to_string()
            .contains("too many attachments"));
    }

    #[test]
    fn bodystructure_transcript_retains_literal_parameters_before_later_header_and_body_literals() {
        let response = ImapResponse {
            lines: vec![
                "* 1 FETCH (BODYSTRUCTURE (\"APPLICATION\" \"PDF\" (\"NAME\" {19}\r\n".into(),
                ") NIL NIL \"BASE64\" 4 NIL (\"ATTACHMENT\" (\"FILENAME\" {19}\r\n".into(),
                "))) BODY[HEADER.FIELDS (SUBJECT)] {17}\r\n".into(),
                " BODY[1] {4}\r\n".into(),
                ")\r\n".into(),
                "D0001 OK Fetch complete\r\n".into(),
            ],
            literals: vec![
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::BodyStructure,
                    bytes: b"claim-documents.pdf".to_vec(),
                },
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::BodyStructure,
                    bytes: b"claim-documents.pdf".to_vec(),
                },
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::Body("BODY[HEADER.FIELDS (SUBJECT)]".into()),
                    bytes: b"Subject: Test\r\n\r\n".to_vec(),
                },
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::Body("BODY[1]".into()),
                    bytes: b"body".to_vec(),
                },
            ],
        };
        let structure = parse_bodystructure_response(&response).unwrap();

        assert_eq!(structure.filename(), Some("claim-documents.pdf"));
        assert_eq!(response.literals.len(), 4);
        let mut response = response;
        assert_eq!(
            response.take_header_literal_for(1).as_deref(),
            Some(b"Subject: Test\r\n\r\n".as_slice())
        );
        assert_eq!(
            response.take_body_literal_for(1, "1").as_deref(),
            Some(b"body".as_slice())
        );
    }

    #[test]
    fn header_literal_selection_survives_reverse_order_before_bodystructure_literals() {
        let mut response = ImapResponse {
            lines: vec![
                "* 1 FETCH (BODY[HEADER.FIELDS (SUBJECT)] {17}\r\n".into(),
                " BODYSTRUCTURE (\"APPLICATION\" \"PDF\" (\"NAME\" {19}\r\n".into(),
                ") NIL NIL \"BASE64\" 4 NIL (\"ATTACHMENT\" (\"FILENAME\" \"claim-documents.pdf\")))\r\n".into(),
                "D0001 OK Fetch complete\r\n".into(),
            ],
            literals: vec![
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::Body("BODY[HEADER.FIELDS (SUBJECT)]".into()),
                    bytes: b"Subject: Test\r\n\r\n".to_vec(),
                },
                ImapLiteral {
                    uid: Some(1),
                    data_item: ImapLiteralItem::BodyStructure,
                    bytes: b"claim-documents.pdf".to_vec(),
                },
            ],
        };

        let structure = parse_bodystructure_response(&response).unwrap();
        assert_eq!(structure.filename(), Some("claim-documents.pdf"));
        assert_eq!(
            response.take_header_literal_for(1).as_deref(),
            Some(b"Subject: Test\r\n\r\n".as_slice())
        );
    }

    #[test]
    fn bodystructure_locator_ignores_header_literal_text() {
        let response = ImapResponse {
            lines: vec![
                "* 1 FETCH (BODY[HEADER.FIELDS (SUBJECT)] {28}\r\n".into(),
                " BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 4 1))\r\n".into(),
            ],
            literals: vec![ImapLiteral {
                uid: Some(42),
                data_item: ImapLiteralItem::Body("BODY[HEADER.FIELDS (SUBJECT)]".into()),
                bytes: b"Subject: BODYSTRUCTURE\r\n\r\n".to_vec(),
            }],
        };
        let structure = parse_bodystructure_response(&response).unwrap();
        assert_eq!(structure.mime_type, "text/plain");
    }

    #[test]
    fn literal_labels_ignore_body_tokens_inside_quoted_parameters() {
        assert_eq!(
            literal_data_item(
                "* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"NAME\" \"BODY[HEADER]\") NIL NIL \"7BIT\" {4}\r\n"
            ),
            ImapLiteralItem::BodyStructure
        );
        assert_eq!(
            literal_data_item("* 1 FETCH (BODYSTRUCTURE NIL BODY[HEADER.FIELDS (SUBJECT)] {4}\r\n"),
            ImapLiteralItem::Body("BODY[HEADER.FIELDS (SUBJECT)]".into())
        );
    }

    #[test]
    fn literal_selection_requires_the_requested_fetch_uid_and_body_section() {
        let mut response = ImapResponse {
            lines: vec![],
            literals: vec![
                ImapLiteral {
                    uid: Some(7),
                    data_item: ImapLiteralItem::Body("BODY[1]".into()),
                    bytes: b"unsolicited-other-message".to_vec(),
                },
                ImapLiteral {
                    uid: Some(42),
                    data_item: ImapLiteralItem::Body("BODY[2]".into()),
                    bytes: b"unsolicited-other-section".to_vec(),
                },
                ImapLiteral {
                    uid: Some(42),
                    data_item: ImapLiteralItem::Body("BODY[1]".into()),
                    bytes: b"requested".to_vec(),
                },
            ],
        };
        assert_eq!(
            response.take_body_literal_for(42, "1").as_deref(),
            Some(b"requested".as_slice())
        );
        assert_eq!(response.literals.len(), 2);
        assert_eq!(
            fetch_uid_from_line("* 3 FETCH (UID 42 BODY[1] {9}\r\n"),
            Some(42)
        );
    }

    #[tokio::test]
    async fn scripted_imap_transcript_handles_fragmented_literals_and_unsolicited_fetches() {
        let (mut client, server) = plain_imap_client_and_server().await;
        let server = tokio::spawn(async move {
            let (read, mut write) = server.into_split();
            let mut read = BufReader::new(read);
            let (tag, command) = scripted_command(&mut read).await;
            assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");

            // Each protocol unit is intentionally split differently. The
            // final requested literal appears before its UID, while the two
            // preceding literals belong to unsolicited/wrong FETCH replies.
            write_transcript_fragments(
                &mut write,
                &[
                    b"* 1 FETCH (UID 7 BODY[1] {5}\r\nwr",
                    b"ong)\r\n* 2 FETCH (UID 42 BODY[2] {5}\r\nother)\r\n",
                    b"* 3 FETCH (BODY[1] {5}\r\nright UID 42)\r\n",
                    format!("{tag} OK FETCH complete\r\n").as_bytes(),
                ],
            )
            .await;
        });

        let mut response = client
            .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(
            response.take_body_literal_for(42, "1").as_deref(),
            Some(b"right".as_slice())
        );
        assert_eq!(response.literals.len(), 2);
        assert!(response
            .literals
            .iter()
            .any(|literal| literal.uid == Some(7)));
        assert!(response.literals.iter().any(|literal| {
            literal.uid == Some(42) && literal.data_item == ImapLiteralItem::Body("BODY[2]".into())
        }));
        assert_eq!(
            response
                .lines
                .iter()
                .filter(|line| line.starts_with("D0001 "))
                .count(),
            1,
            "one tagged wire response must produce one transcript line"
        );
    }

    #[tokio::test]
    async fn scripted_imap_rejects_bye_eof_and_oversize_literal_before_reading_it() {
        let (mut bye_client, bye_server) = plain_imap_client_and_server().await;
        let bye_server = tokio::spawn(async move {
            let (read, mut write) = bye_server.into_split();
            let mut read = BufReader::new(read);
            let (_, command) = scripted_command(&mut read).await;
            assert_eq!(command, "NOOP");
            write_transcript_fragments(&mut write, &[b"* B", b"YE maintenance\r\n"]).await;
        });
        let bye = bye_client.command("NOOP").await.unwrap_err();
        bye_server.await.unwrap();
        assert!(bye
            .to_string()
            .contains("IMAP server closed the connection: * BYE maintenance"));

        let (mut eof_client, eof_server) = plain_imap_client_and_server().await;
        let eof_server = tokio::spawn(async move {
            let (read, _) = eof_server.into_split();
            let mut read = BufReader::new(read);
            let (_, command) = scripted_command(&mut read).await;
            assert_eq!(command, "NOOP");
        });
        let eof = eof_client.command("NOOP").await.unwrap_err();
        eof_server.await.unwrap();
        assert!(eof
            .to_string()
            .contains("IMAP connection closed during command"));

        let (mut limited_client, limited_server) = plain_imap_client_and_server().await;
        let limited_server = tokio::spawn(async move {
            let (read, mut write) = limited_server.into_split();
            let mut read = BufReader::new(read);
            let (tag, command) = scripted_command(&mut read).await;
            assert_eq!(command, "UID FETCH 1 (BODY.PEEK[])");
            write_transcript_fragments(
                &mut write,
                &[format!("* 1 FETCH (UID 1 BODY[] {{4}}\r\n{tag} OK ignored\r\n").as_bytes()],
            )
            .await;
        });
        let limited = match limited_client
            .command_with_literal_limited("UID FETCH 1 (BODY.PEEK[])", 3)
            .await
        {
            Ok(_) => panic!("oversize literal unexpectedly completed"),
            Err(error) => error,
        };
        limited_server.await.unwrap();
        assert!(limited
            .to_string()
            .contains("IMAP response literals exceed the 0 MiB safety limit"));
    }

    #[tokio::test]
    async fn scripted_imap_idle_honours_cancellation_and_completes_the_done_exchange() {
        let (mut client, server) = plain_imap_client_and_server().await;
        let server = tokio::spawn(async move {
            let (read, mut write) = server.into_split();
            let mut read = BufReader::new(read);
            let (tag, command) = scripted_command(&mut read).await;
            assert_eq!(command, "IDLE");
            write_transcript_fragments(&mut write, &[b"+ id", b"ling\r\n"]).await;
            let mut done = String::new();
            read.read_line(&mut done).await.unwrap();
            assert_eq!(done, "DONE\r\n");
            write_transcript_fragments(
                &mut write,
                &[format!("{tag} OK IDLE completed\r\n").as_bytes()],
            )
            .await;
        });

        let (cancel, mut cancelled) = watch::channel(false);
        let idle = tokio::spawn(async move { client.idle_until_change(&mut cancelled).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.send(true).unwrap();

        assert!(matches!(
            idle.await.unwrap().unwrap(),
            IdleOutcome::Cancelled
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn scripted_imap_idle_eof_and_bye_termination_fail_without_spinning() {
        let (mut eof_client, eof_server) = plain_imap_client_and_server().await;
        let eof_server = tokio::spawn(async move {
            let (read, _) = eof_server.into_split();
            let mut read = BufReader::new(read);
            let (_, command) = scripted_command(&mut read).await;
            assert_eq!(command, "IDLE");
        });
        let (_cancel, mut not_cancelled) = watch::channel(false);
        let eof = timeout(
            Duration::from_secs(1),
            eof_client.idle_until_change(&mut not_cancelled),
        )
        .await
        .expect("IDLE EOF must not spin")
        .unwrap_err();
        eof_server.await.unwrap();
        assert!(eof
            .to_string()
            .contains("IMAP connection closed before IDLE continuation"));

        let (mut bye_client, bye_server) = plain_imap_client_and_server().await;
        let bye_server = tokio::spawn(async move {
            let (read, mut write) = bye_server.into_split();
            let mut read = BufReader::new(read);
            let (_, command) = scripted_command(&mut read).await;
            assert_eq!(command, "IDLE");
            write.write_all(b"+ idling\r\n").await.unwrap();
            write.flush().await.unwrap();
            let mut done = String::new();
            read.read_line(&mut done).await.unwrap();
            assert_eq!(done, "DONE\r\n");
            write.write_all(b"* BYE maintenance\r\n").await.unwrap();
            write.flush().await.unwrap();
        });
        let (cancel, mut cancelled) = watch::channel(false);
        let idle = tokio::spawn(async move { bye_client.idle_until_change(&mut cancelled).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.send(true).unwrap();
        let bye = timeout(Duration::from_secs(1), idle)
            .await
            .expect("IDLE BYE termination must not spin")
            .unwrap()
            .unwrap_err();
        bye_server.await.unwrap();
        assert!(bye
            .to_string()
            .contains("IMAP server closed the connection during IDLE termination"));
    }

    #[tokio::test]
    async fn authentication_distinguishes_rejection_from_retryable_bye() {
        let account = test_account();

        let (mut bye_client, bye_server) = plain_imap_client_and_server().await;
        let bye_server = tokio::spawn(async move {
            let (read, mut write) = bye_server.into_split();
            let mut read = BufReader::new(read);
            let (_, command) = scripted_command(&mut read).await;
            assert!(command.starts_with("LOGIN ") || command.starts_with("AUTHENTICATE XOAUTH2 "));
            write
                .write_all(b"* BYE [UNAVAILABLE] maintenance\r\n")
                .await
                .unwrap();
            write.flush().await.unwrap();
        });
        let bye = bye_client
            .authenticate(&account, "secret")
            .await
            .unwrap_err();
        bye_server.await.unwrap();
        assert!(!bye
            .to_string()
            .to_ascii_lowercase()
            .contains("authentication"));

        let (mut rejected_client, rejected_server) = plain_imap_client_and_server().await;
        let rejected_server = tokio::spawn(async move {
            let (read, mut write) = rejected_server.into_split();
            let mut read = BufReader::new(read);
            let (tag, command) = scripted_command(&mut read).await;
            assert!(command.starts_with("LOGIN ") || command.starts_with("AUTHENTICATE XOAUTH2 "));
            write
                .write_all(format!("{tag} NO invalid credentials\r\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
        });
        let rejected = rejected_client
            .authenticate(&account, "secret")
            .await
            .unwrap_err();
        rejected_server.await.unwrap();
        assert!(rejected
            .to_string()
            .contains("IMAP authentication rejected"));
    }

    #[test]
    fn uid_after_a_body_literal_backfills_only_that_fetch_response() {
        let mut literals = vec![ImapLiteral {
            uid: None,
            data_item: ImapLiteralItem::Body("BODY[TEXT]".into()),
            bytes: b"requested-root-body".to_vec(),
        }];
        let mut pending = vec![0];
        backfill_fetch_literal_uids(&mut literals, &mut pending, 42);
        assert_eq!(literals[0].uid, Some(42));
        let mut response = ImapResponse {
            lines: vec![],
            literals,
        };
        assert_eq!(
            response.take_body_literal_for(42, "TEXT").as_deref(),
            Some(b"requested-root-body".as_slice())
        );
        assert!(is_untagged_fetch_start("* 7 FETCH (BODY[TEXT] {19}\r\n"));
        assert!(is_untagged_fetch_start("* 8 FETCH (UID 7 BODY[1] {4}\r\n"));
    }

    #[test]
    fn aggregate_literal_budget_rejects_many_small_literals_before_allocation() {
        assert!(reserve_imap_literal_budget(3, 24, 9, 32).is_err());
        assert!(reserve_imap_literal_budget(MAX_IMAP_RESPONSE_LITERALS, 0, 1, 32).is_err());
        assert!(reserve_imap_literal_budget(3, 24, 8, 32).is_ok());
    }

    #[test]
    fn response_transcript_budget_rejects_bodystructure_growth_before_joining_lines() {
        assert_eq!(
            reserve_imap_transcript_budget(MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES - 1, 1).unwrap(),
            MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES
        );
        assert!(reserve_imap_transcript_budget(MAX_IMAP_RESPONSE_TRANSCRIPT_BYTES, 1).is_err());
    }

    #[test]
    fn selective_fetch_rejects_recycled_uids_when_catalogue_uidvalidity_differs() {
        let error = verify_mailbox_uid_validity(8, 7, "opening").unwrap_err();
        assert!(error.to_string().contains("mailbox identity changed"));
        assert!(error.to_string().contains("sync the account"));
        assert!(verify_mailbox_uid_validity(7, 7, "opening").is_ok());
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
        // SMTP acceptance is not a durable Sent-copy guarantee for generic
        // providers. Only this exact Gmail submission host opts out of the
        // subsequent IMAP APPEND path.
        gmail.smtp_host = "SMTP.GMAIL.COM.".into();
        assert!(smtp_saves_sent_copy(&gmail));
        gmail.smtp_host = "smtp.gmail.com.example.test".into();
        assert!(!smtp_saves_sent_copy(&gmail));
        gmail.smtp_host = "smtp.example.com".into();
        assert!(!smtp_saves_sent_copy(&gmail));
    }

    #[test]
    fn extracts_literal_lengths() {
        assert_eq!(literal_length("* 1 FETCH (RFC822 {42}\r\n"), Some(42));
        assert_eq!(literal_length("* LIST (\\Sent) \"/\" \"{5}\"\r\n"), None);
        assert_eq!(literal_length("* OK server says {5} later\r\n"), None);
        assert_eq!(literal_length("* 1 FETCH (BODY[] {5+}\r\n"), Some(5));
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
        let contract: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../apps/desktop/testdata/tauri-contracts/high-risk.json"
        )))
        .unwrap();
        let provider_content = &contract["messageContent"]["providerSignature"];
        assert_eq!(provider_content["body_text"], message.body_text);
        assert_eq!(
            provider_content["body_html"],
            serde_json::to_value(&message.body_html).unwrap()
        );
        assert_eq!(
            provider_content["attachments"][0]["filename"],
            message.attachments[0].attachment.filename
        );
        assert_eq!(
            provider_content["attachments"][0]["mime_type"],
            message.attachments[0].attachment.mime_type
        );
        assert_eq!(
            provider_content["attachments"][0]["presentation"],
            serde_json::to_value(message.attachments[0].attachment.presentation).unwrap()
        );

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
