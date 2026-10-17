mod agent_bridge;
mod agent_commands;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use dakia_core::drafts::DraftService;
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    AccountDraft, AccountService, ComposeMessage, ConversationTarget, EmailClassificationInput,
    LocalEmailClassifier, MailCursor, MailService, MailboxAction, ModelClassificationUpdate,
    SearchQuery, Security, Store, ThreadContextKind,
};
use directories::ProjectDirs;
use secrecy::SecretString;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    io::{self, IsTerminal, Read},
    path::PathBuf,
};
use url::Url;
use uuid::Uuid;

const REMOTE_SEARCH_CONCURRENCY: usize = 4;
const CLASSIFICATION_BATCH_SIZE: usize = 64;

#[derive(Debug)]
struct PartialFailure {
    message: String,
    details: serde_json::Value,
}

impl std::fmt::Display for PartialFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PartialFailure {}

#[derive(Parser)]
#[command(
    name = "dakia",
    version,
    about = "Search, read, and send mail from the terminal"
)]
struct Cli {
    #[arg(long, env = "DAKIA_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    Sync(SyncArgs),
    /// Keep synchronizing selected accounts until interrupted.
    Watch(WatchArgs),
    Classify(ClassifyArgs),
    Search(SearchArgs),
    Show(ShowArgs),
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    Archive(MailboxActionArgs),
    Spam(MailboxActionArgs),
    Trash(ConfirmedMailboxActionArgs),
    Delete(ConfirmedMailboxActionArgs),
    Send(SendArgs),
    /// Manage local, provider-backed drafts.
    Draft {
        #[command(subcommand)]
        command: DraftCommand,
    },
    /// Create a local reply draft for a stored message.
    Reply(DraftReplyArgs),
    /// Create a local forward draft for a stored message.
    Forward(DraftForwardArgs),
    /// Read and change messages using stable Dakia message IDs.
    Message {
        #[command(subcommand)]
        command: MessageCommand,
    },
    /// Manage IMAP folders.
    Folder {
        #[command(subcommand)]
        command: FolderCommand,
    },
    /// Inspect conversations and their saved local context.
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Store account-local free-text notes and agent memory for a conversation.
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Start the bundled Dakia email agent.
    Agent(agent_commands::AgentArgs),
    /// Connect and inspect AI providers used by the Dakia agent.
    Provider(agent_commands::ProviderArgs),
    #[command(hide = true)]
    AgentBridge,
    Ai {
        #[command(subcommand)]
        command: AiCommand,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    List,
    /// Add an IMAP/SMTP account using a preset or custom endpoints.
    Add(AccountAddArgs),
    /// Change one stored account. Omitted fields keep their current values.
    Edit(AccountEditArgs),
    /// Remove an account, its encrypted credential, and local mail cache.
    Remove(AccountRemoveArgs),
    /// Verify IMAP and SMTP authentication without reading or sending mail.
    Test(AccountIdArgs),
    Enable(AccountIdArgs),
    Disable(AccountIdArgs),
    /// Print built-in IMAP/SMTP presets.
    Presets,
}

#[derive(Args)]
struct AccountIdArgs {
    #[arg(long)]
    account: Uuid,
}

#[derive(Args)]
struct AccountAddArgs {
    #[arg(long)]
    email: Option<String>,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    account_name: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    imap_host: Option<String>,
    #[arg(long)]
    imap_port: Option<u16>,
    #[arg(long, value_enum)]
    imap_security: Option<CliSecurity>,
    #[arg(long)]
    smtp_host: Option<String>,
    #[arg(long)]
    smtp_port: Option<u16>,
    #[arg(long, value_enum)]
    smtp_security: Option<CliSecurity>,
    #[arg(long)]
    archive_mailbox: Option<String>,
    #[arg(long)]
    spam_mailbox: Option<String>,
    #[command(flatten)]
    secret: SecretInputArgs,
}

#[derive(Args)]
struct AccountEditArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    email: Option<String>,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    account_name: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    imap_host: Option<String>,
    #[arg(long)]
    imap_port: Option<u16>,
    #[arg(long, value_enum)]
    imap_security: Option<CliSecurity>,
    #[arg(long)]
    smtp_host: Option<String>,
    #[arg(long)]
    smtp_port: Option<u16>,
    #[arg(long, value_enum)]
    smtp_security: Option<CliSecurity>,
    #[arg(long)]
    archive_mailbox: Option<String>,
    #[arg(long)]
    spam_mailbox: Option<String>,
    #[command(flatten)]
    secret: OptionalSecretInputArgs,
}

#[derive(Args)]
struct AccountRemoveArgs {
    #[arg(long)]
    account: Uuid,
    /// Required because this permanently removes the local account and cache.
    #[arg(long)]
    yes: bool,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum CliSecurity {
    Tls,
    Starttls,
}

impl From<CliSecurity> for Security {
    fn from(value: CliSecurity) -> Self {
        match value {
            CliSecurity::Tls => Security::Tls,
            CliSecurity::Starttls => Security::StartTls,
        }
    }
}

#[derive(Args)]
struct SecretInputArgs {
    /// Read the password or app password from standard input.
    #[arg(long, conflicts_with = "password_env")]
    password_stdin: bool,
    /// Read the password or app password from this environment variable.
    #[arg(long, value_name = "NAME", conflicts_with = "password_stdin")]
    password_env: Option<String>,
}

#[derive(Args, Default)]
struct OptionalSecretInputArgs {
    #[arg(long, conflicts_with = "password_env")]
    password_stdin: bool,
    #[arg(long, value_name = "NAME", conflicts_with = "password_stdin")]
    password_env: Option<String>,
}

#[derive(Args)]
struct WatchArgs {
    #[arg(long)]
    account: Vec<Uuid>,
    #[arg(long, default_value_t = 250)]
    limit: u32,
}

#[derive(Args)]
struct SyncArgs {
    #[arg(long)]
    account: Option<Uuid>,
    /// Synchronize one IMAP mailbox instead of the account's standard folders.
    #[arg(long, conflicts_with = "refresh_gmail_categories")]
    folder: Option<String>,
    #[arg(long, default_value_t = 250)]
    limit: u32,
    /// Refresh only Gmail's built-in category metadata for already-synced mail.
    /// This does not download bodies or reset incremental-sync state.
    #[arg(long)]
    refresh_gmail_categories: bool,
}

#[derive(Args)]
struct ClassifyArgs {
    /// Directory containing the bundled `model.onnx` and tokenizer assets.
    #[arg(long)]
    model_dir: PathBuf,
    /// Reclassify every message previously classified by the local model.
    /// User-selected categories are always preserved.
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct SearchArgs {
    /// Full-text query. Omit to list recent mail.
    #[arg(default_value = "")]
    query: String,
    #[arg(long)]
    account: Vec<Uuid>,
    #[arg(long)]
    mailbox: Option<String>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    unread: bool,
    #[arg(long)]
    read: bool,
    #[arg(long)]
    flagged: bool,
    #[arg(long)]
    unflagged: bool,
    #[arg(long)]
    category: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: u32,
    /// Continue after this `received_at,id` cursor returned by JSON output.
    #[arg(long)]
    cursor: Option<String>,
    /// Query the provider as well as the local catalogue. Remote results are
    /// saved as metadata so they are immediately available in the desktop app.
    #[arg(long)]
    remote: bool,
}

#[derive(Args)]
struct ShowArgs {
    /// The stable Dakia message ID returned by `search`.
    message_id: String,
}

#[derive(Subcommand)]
enum AttachmentCommand {
    List(ShowArgs),
    Download(DownloadAttachmentArgs),
}

#[derive(Args)]
struct DownloadAttachmentArgs {
    /// The stable Dakia message ID returned by `search`.
    message_id: String,
    /// The attachment ID returned by `attachment list`.
    attachment_id: String,
    /// A new output file. Existing files are never overwritten.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Args)]
struct MailboxActionArgs {
    /// One or more stable Dakia message IDs returned by `search`.
    #[arg(required = true)]
    message_id: Vec<String>,
}

#[derive(Args)]
struct ConfirmedMailboxActionArgs {
    #[command(flatten)]
    messages: MailboxActionArgs,
    /// Required because moving mail to Trash and permanent deletion are
    /// destructive mailbox operations.
    #[arg(long)]
    yes: bool,
}

#[derive(Args)]
struct SendArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long, required = true)]
    to: Vec<String>,
    #[arg(long)]
    cc: Vec<String>,
    #[arg(long)]
    bcc: Vec<String>,
    #[arg(long)]
    subject: String,
    #[arg(long, help = "Body text; omit to read from stdin")]
    body: Option<String>,
    /// Optional HTML alternative for mail clients that support rich text.
    #[arg(long)]
    html_body: Option<String>,
    /// Add a Reply-To relationship to an existing RFC Message-ID.
    #[arg(long)]
    in_reply_to: Option<String>,
    /// Add one or more RFC Message-IDs to the References header.
    #[arg(long)]
    references: Option<String>,
    /// Attach a regular file. May be repeated; each file is limited to 25 MiB
    /// and the combined attachment size to 50 MiB.
    #[arg(long = "attach", value_name = "PATH")]
    attachments: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum FolderCommand {
    List(AccountIdArgs),
    Create(FolderNameArgs),
    Rename(FolderRenameArgs),
    Delete(FolderDeleteArgs),
}

#[derive(Args)]
struct FolderNameArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    name: String,
}

#[derive(Args)]
struct FolderRenameArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    name: String,
    #[arg(long)]
    new_name: String,
}

#[derive(Args)]
struct FolderDeleteArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    name: String,
    #[arg(long)]
    yes: bool,
}

#[derive(Subcommand)]
enum MessageCommand {
    List(MessageListArgs),
    Read(ShowArgs),
    Search(SearchArgs),
    Flag(MessageFlagArgs),
    Move(MessageDestinationArgs),
    Copy(MessageDestinationArgs),
    Import(MessageImportArgs),
    Export(MessageExportArgs),
}

#[derive(Args)]
struct MessageListArgs {
    #[command(flatten)]
    search: SearchArgs,
}

#[derive(Args)]
struct MessageFlagArgs {
    #[arg(required = true)]
    message_id: Vec<String>,
    #[arg(long, conflicts_with = "unflag")]
    flag: bool,
    #[arg(long, conflicts_with = "flag")]
    unflag: bool,
    #[arg(long, conflicts_with = "unread")]
    read: bool,
    #[arg(long, conflicts_with = "read")]
    unread: bool,
    /// Additional closed-set IMAP system flag to set. May be repeated.
    #[arg(long = "system-flag", value_enum)]
    system_flags: Vec<CliMessageFlag>,
    /// Additional closed-set IMAP system flag to clear. May be repeated.
    #[arg(long = "clear-system-flag", value_enum)]
    clear_system_flags: Vec<CliMessageFlag>,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum CliMessageFlag {
    Seen,
    Flagged,
    Answered,
    Draft,
    Deleted,
}

impl From<CliMessageFlag> for dakia_core::mail::MessageFlag {
    fn from(value: CliMessageFlag) -> Self {
        match value {
            CliMessageFlag::Seen => Self::Seen,
            CliMessageFlag::Flagged => Self::Flagged,
            CliMessageFlag::Answered => Self::Answered,
            CliMessageFlag::Draft => Self::Draft,
            CliMessageFlag::Deleted => Self::Deleted,
        }
    }
}

#[derive(Args)]
struct MessageDestinationArgs {
    #[arg(required = true)]
    message_id: Vec<String>,
    #[arg(long)]
    destination: String,
}

#[derive(Args)]
struct MessageImportArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    mailbox: String,
    /// RFC 822 `.eml` input. Omit to read raw bytes from stdin.
    #[arg(long)]
    input: Option<PathBuf>,
}

#[derive(Args)]
struct MessageExportArgs {
    #[arg(long)]
    message_id: String,
    /// New `.eml` output path. Existing files are never overwritten.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Subcommand)]
enum DraftCommand {
    List(DraftListArgs),
    Show(DraftIdArgs),
    Create(DraftCreateArgs),
    Edit(DraftEditArgs),
    Delete(DraftDeleteArgs),
    Reply(DraftReplyArgs),
    Forward(DraftForwardArgs),
    Send(DraftSendArgs),
}

#[derive(Args)]
struct DraftListArgs {
    #[arg(long)]
    account: Option<Uuid>,
}
#[derive(Args)]
struct DraftIdArgs {
    #[arg(long)]
    draft_id: Uuid,
}
#[derive(Args)]
struct DraftDeleteArgs {
    #[arg(long)]
    draft_id: Uuid,
    #[arg(long)]
    yes: bool,
}
#[derive(Args)]
struct DraftCreateArgs {
    #[arg(long)]
    account: Uuid,
    #[arg(long)]
    to: Vec<String>,
    #[arg(long)]
    cc: Vec<String>,
    #[arg(long)]
    bcc: Vec<String>,
    #[arg(long)]
    subject: String,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    html_body: Option<String>,
    #[arg(long = "attach")]
    attachments: Vec<PathBuf>,
}
#[derive(Args)]
struct DraftEditArgs {
    #[arg(long)]
    draft_id: Uuid,
    /// Current local draft revision returned by `draft show`.
    #[arg(long)]
    revision: i64,
    #[arg(long)]
    to: Option<Vec<String>>,
    #[arg(long)]
    cc: Option<Vec<String>>,
    #[arg(long)]
    bcc: Option<Vec<String>>,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    html_body: Option<String>,
}
#[derive(Args)]
struct DraftReplyArgs {
    #[arg(long)]
    message_id: String,
    #[arg(long)]
    body: Option<String>,
}
#[derive(Args)]
struct DraftForwardArgs {
    #[arg(long)]
    message_id: String,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    attach_original: bool,
}
#[derive(Args)]
struct DraftSendArgs {
    #[arg(long)]
    draft_id: Uuid,
}

#[derive(Args)]
struct ThreadArgs {
    #[arg(long)]
    account: Uuid,
    /// A stable local Dakia message ID, RFC Message-ID, or stored thread ID.
    #[arg(long, conflicts_with_all = ["rfc_message_id", "thread_id"])]
    message_id: Option<String>,
    #[arg(long, conflicts_with_all = ["message_id", "thread_id"])]
    rfc_message_id: Option<String>,
    #[arg(long, conflicts_with_all = ["message_id", "rfc_message_id"])]
    thread_id: Option<String>,
    #[arg(long)]
    mailbox: Option<String>,
}

#[derive(Subcommand)]
enum ContextCommand {
    Get(ContextTargetArgs),
    Set(ContextWriteArgs),
    Append(ContextWriteArgs),
    Clear(ContextClearArgs),
    History(ContextTargetArgs),
    Restore(ContextRestoreArgs),
}

#[derive(Subcommand)]
enum ThreadCommand {
    Show(ThreadArgs),
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum ContextKind {
    Notes,
    #[value(name = "agent_memory", alias = "agent-memory")]
    AgentMemory,
}

#[derive(Args)]
struct ContextTargetArgs {
    #[command(flatten)]
    target: ThreadArgs,
    /// Permanent Dakia conversation ID returned by prior context output.
    /// It remains valid after source mail is removed.
    #[arg(long, conflicts_with_all = ["message_id", "rfc_message_id", "thread_id", "mailbox"])]
    conversation_id: Option<Uuid>,
    #[arg(long, value_enum, default_value_t = ContextKind::Notes)]
    kind: ContextKind,
}
#[derive(Args)]
struct ContextWriteArgs {
    #[command(flatten)]
    target: ContextTargetArgs,
    #[arg(long)]
    text: Option<String>,
    /// Required document revision. Use 0 for a new document.
    #[arg(long)]
    revision: u64,
}
#[derive(Args)]
struct ContextClearArgs {
    #[command(flatten)]
    target: ContextTargetArgs,
    #[arg(long)]
    revision: u64,
}
#[derive(Args)]
struct ContextRestoreArgs {
    #[command(flatten)]
    target: ContextTargetArgs,
    #[arg(long)]
    revision_id: Uuid,
    #[arg(long)]
    revision: u64,
}

#[derive(Subcommand)]
enum AiCommand {
    Summarize(AiMessagesArgs),
}

#[derive(Args)]
struct AiMessagesArgs {
    #[arg(required = true)]
    message_id: Vec<String>,
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    let requested_json = std::env::args_os().any(|argument| {
        argument
            .to_str()
            .is_some_and(|argument| argument == "--json")
    });
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if requested_json => {
            let message = error.to_string();
            println!(
                "{}",
                serde_json::json!({"error":{"code":"invalid_arguments","message":message}})
            );
            eprint!("{error}");
            std::process::exit(2);
        }
        Err(error) => error.exit(),
    };
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    let json = cli.json;
    let result = match cli.command {
        Command::Agent(args) => agent_commands::run_agent(data_dir, args, json).await,
        Command::Provider(args) => agent_commands::run_provider(data_dir, args, json).await,
        Command::AgentBridge => agent_bridge::run(data_dir).await,
        command => run_mail_command(data_dir, json, command).await,
    };
    if let Err(error) = result {
        let (code, exit) = cli_error(&error);
        if json {
            let payload = if let Some(partial) = error.downcast_ref::<PartialFailure>() {
                serde_json::json!({
                    "error": {
                        "code": code,
                        "message": partial.message,
                        "details": partial.details,
                    }
                })
            } else {
                serde_json::json!({"error":{"code":code,"message":error.to_string()}})
            };
            println!("{}", payload);
        }
        eprintln!("Error: {}", error);
        std::process::exit(exit);
    }
}

fn cli_error(error: &anyhow::Error) -> (&'static str, i32) {
    if error.downcast_ref::<PartialFailure>().is_some() {
        return ("partial_failure", 8);
    }
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("not found") || message.contains("does not exist") {
        ("not_found", 3)
    } else if message.contains("changed in another")
        || message.contains("expected revision")
        || message.contains("settings changed since")
    {
        ("conflict", 4)
    } else if message.contains("credential")
        || message.contains("authentication")
        || message.contains("password")
    {
        ("authentication", 5)
    } else if message.contains("operation is active") || message.contains("busy") {
        ("busy", 6)
    } else if message.contains("imap")
        || message.contains("smtp")
        || message.contains("provider")
        || message.contains("sync failed")
    {
        ("provider", 7)
    } else {
        ("operation_failed", 1)
    }
}

async fn run_mail_command(data_dir: PathBuf, json: bool, command: Command) -> Result<()> {
    let store = Store::open(data_dir.join("dakia.db")).await?;
    match command {
        Command::Account { command } => match command {
            command => run_account_command(&store, command, json).await?,
        },
        Command::Sync(args) => {
            let mail = MailService::new(store.clone());
            let all_accounts = store.accounts().await?;
            let accounts = all_accounts
                .into_iter()
                .filter(|item| item.enabled)
                .filter(|item| args.account.map(|id| id == item.id).unwrap_or(true))
                .collect::<Vec<_>>();
            if args.account.is_some() && accounts.is_empty() {
                bail!("account not found");
            }
            for account in accounts {
                let count = if args.refresh_gmail_categories {
                    if account.provider_id != "gmail" {
                        if args.account.is_some() {
                            bail!("Gmail category refresh requires a Gmail account");
                        }
                        continue;
                    }
                    mail.refresh_gmail_category_metadata(&account)
                        .await
                        .with_context(|| {
                            format!("Gmail category refresh failed for {}", account.email)
                        })?
                } else if let Some(folder) = args.folder.as_deref() {
                    mail.sync_mailbox(&account, folder, args.limit)
                        .await
                        .with_context(|| format!("sync failed for {}:{folder}", account.email))?
                } else {
                    mail.sync_all(&account, args.limit)
                        .await
                        .with_context(|| format!("sync failed for {}", account.email))?
                };
                let draft_reconcile = DraftService::new(store.clone(), mail.clone())
                    .reconcile_pending(&account, 20)
                    .await?;
                if json {
                    print_value(
                        &if args.refresh_gmail_categories {
                            serde_json::json!({"account_id":account.id,"refreshed_category_labels":count,"draft_reconcile":draft_reconcile})
                        } else {
                            serde_json::json!({"account_id":account.id,"folder":args.folder.as_deref(),"synced":count,"draft_reconcile":draft_reconcile})
                        },
                        true,
                    )?;
                } else {
                    let action = if args.refresh_gmail_categories {
                        "refreshed Gmail category labels for"
                    } else if args.folder.is_some() {
                        "synced selected folder for"
                    } else {
                        "synced"
                    };
                    println!("{}: {action} {count} messages", account.email);
                }
            }
        }
        Command::Classify(args) => {
            let classified = run_classification(&store, args).await?;
            if json {
                println!("{}", serde_json::json!({"classified" : classified}));
            } else {
                println!("classified {classified} messages with the local ONNX model");
            }
        }
        Command::Search(args) => {
            let query = search_query(&args)?;
            let results = if args.remote {
                search_local_and_remote(&store, &query).await?
            } else {
                store.search(&query).await?
            };
            if json {
                print_value(&results, true)?;
            } else {
                for message in results {
                    println!(
                        "{}\t{}\t{}\t{}",
                        message.id,
                        message.received_at.format("%Y-%m-%d"),
                        message.from_address,
                        message.subject
                    );
                }
            }
        }
        Command::Show(args) => {
            let message = fetch_message(&store, &args.message_id).await?;
            print_value(&message, json)?;
        }
        Command::Attachment { command } => match command {
            AttachmentCommand::List(args) => {
                let message = fetch_message(&store, &args.message_id).await?;
                let attachments = message
                    .attachments
                    .iter()
                    .map(|item| &item.attachment)
                    .collect::<Vec<_>>();
                print_value(&attachments, json)?;
            }
            AttachmentCommand::Download(args) => {
                download_attachment(&store, args, json).await?;
            }
        },
        Command::Archive(args) => {
            apply_mailbox_action(&store, &args.message_id, MailboxAction::Archive, json).await?
        }
        Command::Spam(args) => {
            apply_mailbox_action(&store, &args.message_id, MailboxAction::Spam, json).await?
        }
        Command::Trash(args) => {
            require_confirmation(args.yes, "trash")?;
            apply_mailbox_action(
                &store,
                &args.messages.message_id,
                MailboxAction::Trash,
                json,
            )
            .await?
        }
        Command::Delete(args) => {
            require_confirmation(args.yes, "permanently delete")?;
            apply_mailbox_action(
                &store,
                &args.messages.message_id,
                MailboxAction::Delete,
                json,
            )
            .await?
        }
        Command::Send(args) => {
            let account = store
                .account(args.account)
                .await?
                .context("account not found")?;
            let body = match args.body {
                Some(body) => body,
                None => {
                    let mut body = String::new();
                    io::stdin().read_to_string(&mut body)?;
                    body
                }
            };
            let response = MailService::new(store)
                .send(
                    &account,
                    &ComposeMessage {
                        account_id: account.id,
                        to: args.to,
                        cc: args.cc,
                        bcc: args.bcc,
                        subject: args.subject,
                        body_text: body,
                        body_html: args.html_body,
                        in_reply_to: args.in_reply_to,
                        references: args.references,
                        attachments: read_outbound_attachments(&args.attachments)?,
                    },
                )
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status":"sent","response":response})
                );
            } else {
                println!("Sent: {response}");
            }
        }
        Command::Ai { command } => {
            let ai = ai_from_env()?;
            match command {
                AiCommand::Summarize(args) => println!(
                    "{}",
                    ai.summarize(&store.messages_by_ids(&args.message_id).await?)
                        .await?
                ),
            }
        }
        Command::Watch(args) => run_watch(&store, args, json).await?,
        Command::Message { command } => run_message_command(&store, command, json).await?,
        Command::Folder { command } => run_folder_command(&store, command, json).await?,
        Command::Draft { command } => run_draft_command(&store, command, json).await?,
        Command::Reply(args) => run_draft_command(&store, DraftCommand::Reply(args), json).await?,
        Command::Forward(args) => {
            run_draft_command(&store, DraftCommand::Forward(args), json).await?
        }
        Command::Thread { command } => match command {
            ThreadCommand::Show(args) => run_thread_command(&store, args, json).await?,
            ThreadCommand::Context { command } => {
                run_context_command(&store, command, json).await?
            }
        },
        Command::Context { command } => run_context_command(&store, command, json).await?,
        Command::Agent(_) | Command::Provider(_) | Command::AgentBridge => {
            unreachable!("agent commands dispatch before opening the mail store")
        }
    }
    Ok(())
}

async fn run_account_command(store: &Store, command: AccountCommand, json: bool) -> Result<()> {
    let service = AccountService::new(store.clone());
    match command {
        AccountCommand::List => print_value(&store.accounts().await?, json),
        AccountCommand::Presets => print_value(&dakia_core::provider::all(), json),
        AccountCommand::Add(args) => {
            let (draft, prompted_account_name) = account_draft_from_add(&args)?;
            let secret = read_required_secret(args.secret)?;
            let account = service
                .add_with_account_name(draft, args.account_name.or(prompted_account_name), &secret)
                .await?;
            print_value(&account, json)
        }
        AccountCommand::Edit(args) => {
            let existing = store
                .account(args.account)
                .await?
                .context("account not found")?;
            let account = apply_account_edit(existing, &args)?;
            let secret = read_optional_secret(args.secret)?;
            let account = service.update(account, secret.as_deref()).await?;
            print_value(&account, json)
        }
        AccountCommand::Remove(args) => {
            require_confirmation(args.yes, "remove this account and its local mail cache")?;
            service.remove(args.account).await?;
            print_status(
                json,
                serde_json::json!({"status":"removed", "account_id":args.account}),
                "Removed account",
            )
        }
        AccountCommand::Test(args) => {
            service.probe(args.account).await?;
            print_status(
                json,
                serde_json::json!({"status":"ok", "account_id":args.account}),
                "IMAP and SMTP authentication succeeded",
            )
        }
        AccountCommand::Enable(args) => {
            let account = service.set_enabled(args.account, true).await?;
            print_value(&account, json)
        }
        AccountCommand::Disable(args) => {
            let account = service.set_enabled(args.account, false).await?;
            print_value(&account, json)
        }
    }
}

fn account_draft_from_add(args: &AccountAddArgs) -> Result<(AccountDraft, Option<String>)> {
    let email = required_account_field(args.email.clone(), "Email address")?;
    let display_name = required_account_field(args.display_name.clone(), "Your name")?;
    let detected = dakia_core::provider::detect(&email);
    let provider_id = match &args.provider {
        Some(provider) => provider.clone(),
        None if io::stdin().is_terminal() => {
            let value = prompt_line(&format!("Provider [{}]: ", detected.id))?;
            if value.is_empty() {
                detected.id.to_owned()
            } else {
                value
            }
        }
        None => detected.id.to_owned(),
    };
    let preset = dakia_core::provider::by_id(&provider_id)
        .with_context(|| format!("unknown provider preset: {provider_id}"))?;
    let imap_host = optional_or_prompt(args.imap_host.clone(), "IMAP host", preset.id == "custom")?;
    let smtp_host = optional_or_prompt(args.smtp_host.clone(), "SMTP host", preset.id == "custom")?;
    let account_name = if args.account_name.is_none() && io::stdin().is_terminal() {
        let value = prompt_line(&format!("Account name [{email}]: "))?;
        Some(if value.is_empty() {
            email.clone()
        } else {
            value
        })
    } else {
        None
    };
    Ok((
        AccountDraft {
            email,
            display_name,
            provider_id: Some(provider_id),
            username: args.username.clone(),
            imap_host,
            imap_port: args.imap_port,
            imap_security: args.imap_security.map(Into::into),
            smtp_host,
            smtp_port: args.smtp_port,
            smtp_security: args.smtp_security.map(Into::into),
            archive_mailbox: args.archive_mailbox.clone(),
            spam_mailbox: args.spam_mailbox.clone(),
        },
        account_name,
    ))
}

fn required_account_field(value: Option<String>, label: &str) -> Result<String> {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => Ok(value.trim().to_owned()),
        None if io::stdin().is_terminal() => {
            let value = prompt_line(&format!("{label}: "))?;
            if value.trim().is_empty() { bail!("{label} is required"); }
            Ok(value)
        }
        None => bail!("{label} is required; pass the corresponding option when standard input is not a terminal"),
    }
}

fn optional_or_prompt(
    value: Option<String>,
    label: &str,
    required: bool,
) -> Result<Option<String>> {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => Ok(Some(value.trim().to_owned())),
        None if required && io::stdin().is_terminal() => {
            let value = prompt_line(&format!("{label}: "))?;
            if value.trim().is_empty() {
                bail!("{label} is required for custom accounts");
            }
            Ok(Some(value))
        }
        None if required => {
            bail!("{label} is required for custom accounts when standard input is not a terminal")
        }
        None => Ok(None),
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

fn apply_account_edit(
    mut account: dakia_core::Account,
    args: &AccountEditArgs,
) -> Result<dakia_core::Account> {
    if let Some(provider_id) = &args.provider {
        let preset = dakia_core::provider::by_id(provider_id)
            .with_context(|| format!("unknown provider preset: {provider_id}"))?;
        account.provider_id = preset.id.to_owned();
        account.imap_host = preset.imap_host.to_owned();
        account.imap_port = preset.imap_port;
        account.imap_security = preset.imap_security;
        account.smtp_host = preset.smtp_host.to_owned();
        account.smtp_port = preset.smtp_port;
        account.smtp_security = preset.smtp_security;
        account.archive_mailbox = preset.archive_mailbox.to_owned();
        account.spam_mailbox = preset.spam_mailbox.to_owned();
    }
    if let Some(value) = &args.email {
        account.email = value.trim().to_owned();
    }
    if let Some(value) = &args.display_name {
        account.display_name = value.trim().to_owned();
    }
    if let Some(value) = &args.account_name {
        account.account_name = value.trim().to_owned();
    }
    if let Some(value) = &args.username {
        account.auth = dakia_core::AccountAuth::Password {
            username: value.trim().to_owned(),
        };
    }
    if let Some(value) = &args.imap_host {
        account.imap_host = value.trim().to_owned();
    }
    if let Some(value) = args.imap_port {
        account.imap_port = value;
    }
    if let Some(value) = args.imap_security {
        account.imap_security = value.into();
    }
    if let Some(value) = &args.smtp_host {
        account.smtp_host = value.trim().to_owned();
    }
    if let Some(value) = args.smtp_port {
        account.smtp_port = value;
    }
    if let Some(value) = args.smtp_security {
        account.smtp_security = value.into();
    }
    if let Some(value) = &args.archive_mailbox {
        account.archive_mailbox = value.trim().to_owned();
    }
    if let Some(value) = &args.spam_mailbox {
        account.spam_mailbox = value.trim().to_owned();
    }
    Ok(account)
}

fn read_required_secret(args: SecretInputArgs) -> Result<String> {
    read_secret(args.password_stdin, args.password_env)?
        .context("a password or app password is required")
}

fn read_optional_secret(args: OptionalSecretInputArgs) -> Result<Option<String>> {
    if args.password_stdin || args.password_env.is_some() {
        read_secret(args.password_stdin, args.password_env)
    } else {
        Ok(None)
    }
}

fn read_secret(from_stdin: bool, environment: Option<String>) -> Result<Option<String>> {
    match environment {
        Some(name) => std::env::var(&name)
            .with_context(|| format!("password environment variable {name} is not set"))
            .map(Some),
        None if from_stdin => {
            let mut secret = String::new();
            io::stdin().read_to_string(&mut secret)?;
            Ok(Some(secret.trim_end_matches(['\r', '\n']).to_owned()))
        }
        None if io::stdin().is_terminal() => Ok(Some(rpassword::prompt_password(
            "Password or app password: ",
        )?)),
        None => {
            bail!("password input is not a terminal; use --password-stdin or --password-env NAME")
        }
    }
}

fn print_status(json: bool, value: serde_json::Value, human: &str) -> Result<()> {
    if json {
        print_value(&value, true)
    } else {
        println!("{human}");
        Ok(())
    }
}

fn search_query(args: &SearchArgs) -> Result<SearchQuery> {
    if args.read && args.unread {
        bail!("--read and --unread cannot be used together");
    }
    if args.flagged && args.unflagged {
        bail!("--flagged and --unflagged cannot be used together");
    }
    Ok(SearchQuery {
        text: args.query.clone(),
        account_ids: args.account.clone(),
        mailbox: args.mailbox.clone(),
        from: args.from.clone(),
        unread_only: args.unread,
        read_only: args.read,
        flagged_only: args.flagged,
        unflagged_only: args.unflagged,
        category: args.category.clone(),
        limit: Some(args.limit),
        cursor: args.cursor.as_deref().map(parse_cursor).transpose()?,
    })
}

fn parse_cursor(value: &str) -> Result<MailCursor> {
    let (received_at, id) = value
        .rsplit_once(',')
        .context("cursor must be RECEIVED_AT,ID")?;
    Ok(MailCursor {
        received_at: DateTime::parse_from_rfc3339(received_at)?.with_timezone(&Utc),
        id: id.to_owned(),
    })
}

async fn run_message_command(store: &Store, command: MessageCommand, json: bool) -> Result<()> {
    match command {
        MessageCommand::List(args) => run_search_command(store, &args.search, json).await,
        MessageCommand::Search(args) => run_search_command(store, &args, json).await,
        MessageCommand::Read(args) => {
            print_value(&fetch_message(store, &args.message_id).await?, json)
        }
        MessageCommand::Flag(args) => {
            if !args.flag
                && !args.unflag
                && !args.read
                && !args.unread
                && args.system_flags.is_empty()
                && args.clear_system_flags.is_empty()
            {
                bail!("choose at least one flag operation");
            }
            let mut set = args
                .system_flags
                .iter()
                .copied()
                .map(Into::into)
                .collect::<Vec<dakia_core::mail::MessageFlag>>();
            let mut clear = args
                .clear_system_flags
                .iter()
                .copied()
                .map(Into::into)
                .collect::<Vec<dakia_core::mail::MessageFlag>>();
            if args.flag {
                set.push(dakia_core::mail::MessageFlag::Flagged);
            }
            if args.unflag {
                clear.push(dakia_core::mail::MessageFlag::Flagged);
            }
            if args.read {
                set.push(dakia_core::mail::MessageFlag::Seen);
            }
            if args.unread {
                clear.push(dakia_core::mail::MessageFlag::Seen);
            }
            if set.iter().any(|flag| clear.contains(flag)) {
                bail!("cannot set and clear the same system flag");
            }
            let mut completed = Vec::new();
            let mut errors = Vec::new();
            for id in &args.message_id {
                let result = async {
                    let message = store
                        .message(id)
                        .await?
                        .with_context(|| format!("message not found: {id}"))?;
                    let account_id = Uuid::parse_str(&message.account_id)?;
                    let account = store
                        .account(account_id)
                        .await?
                        .context("account not found")?;
                    let mail = MailService::new(store.clone());
                    let uid =
                        u32::try_from(message.uid).context("stored message has an invalid UID")?;
                    mail.set_flags(&account, &message.mailbox, uid, &set, true)
                        .await?;
                    mail.set_flags(&account, &message.mailbox, uid, &clear, false)
                        .await
                }
                .await;
                match result {
                    Ok(()) => completed.push(id.clone()),
                    Err(error) => errors.push(serde_json::json!({
                        "message_id": id,
                        "message": error.to_string(),
                    })),
                }
            }
            if !errors.is_empty() {
                return Err(partial_failure(
                    "message flag update only completed for some messages",
                    serde_json::json!({"status":"updated","message_ids":completed}),
                    errors,
                ));
            }
            print_status(
                json,
                serde_json::json!({"message_ids":completed,"status":"updated"}),
                "Updated message flags",
            )
        }
        MessageCommand::Move(args) => run_move_or_copy(store, args, false, json).await,
        MessageCommand::Copy(args) => run_move_or_copy(store, args, true, json).await,
        MessageCommand::Import(args) => run_import_message(store, args, json).await,
        MessageCommand::Export(args) => run_export_message(store, args, json).await,
    }
}

async fn run_search_command(store: &Store, args: &SearchArgs, json: bool) -> Result<()> {
    let query = search_query(args)?;
    let results = if args.remote {
        search_local_and_remote(store, &query).await?
    } else {
        store.search(&query).await?
    };
    if json {
        let next_cursor = results
            .last()
            .map(|message| format!("{},{}", message.received_at.to_rfc3339(), message.id));
        print_value(
            &serde_json::json!({"messages":results,"next_cursor":next_cursor}),
            true,
        )
    } else {
        for message in results {
            println!(
                "{}\t{}\t{}\t{}",
                message.id,
                message.received_at.format("%Y-%m-%d"),
                message.from_address,
                message.subject
            );
        }
        Ok(())
    }
}

async fn run_thread_command(store: &Store, args: ThreadArgs, json: bool) -> Result<()> {
    let conversation = store
        .conversation_for_target(&conversation_target(&args))
        .await?
        .context("conversation not found")?;
    print_value(&conversation, json)
}

fn conversation_target(args: &ThreadArgs) -> ConversationTarget {
    ConversationTarget {
        account_id: args.account,
        local_message_id: args.message_id.clone(),
        rfc_message_id: args.rfc_message_id.clone(),
        thread_id: args.thread_id.clone(),
        mailbox: args.mailbox.clone(),
    }
}

async fn run_move_or_copy(
    store: &Store,
    args: MessageDestinationArgs,
    copy: bool,
    json: bool,
) -> Result<()> {
    let mut results = Vec::with_capacity(args.message_id.len());
    let mut errors = Vec::new();
    for message_id in &args.message_id {
        let result = async {
            let message = store
                .message(message_id)
                .await?
                .with_context(|| format!("message not found: {message_id}"))?;
            let account = store
                .account(Uuid::parse_str(&message.account_id)?)
                .await?
                .context("account not found")?;
            let uid = u32::try_from(message.uid).context("stored message has an invalid UID")?;
            if copy {
                MailService::new(store.clone())
                    .copy_message(&account, &message.mailbox, uid, &args.destination)
                    .await
            } else {
                MailService::new(store.clone())
                    .move_message(&account, &message.mailbox, uid, &args.destination)
                    .await
            }
        };
        match result.await {
            Ok(destination_uid) => results.push(serde_json::json!({"message_id":message_id,"destination":args.destination,"destination_uid":destination_uid})),
            Err(error) => errors.push(serde_json::json!({
                "message_id": message_id,
                "message": error.to_string(),
            })),
        }
    }
    if !errors.is_empty() {
        return Err(partial_failure(
            if copy {
                "message copy only completed for some messages"
            } else {
                "message move only completed for some messages"
            },
            serde_json::json!({"results":results}),
            errors,
        ));
    }
    print_value(&results, json)
}

async fn run_import_message(store: &Store, args: MessageImportArgs, json: bool) -> Result<()> {
    let account = store
        .account(args.account)
        .await?
        .context("account not found")?;
    let raw = match args.input {
        Some(path) => {
            fs::read(&path).with_context(|| format!("could not read {}", path.display()))?
        }
        None => {
            let mut raw = Vec::new();
            io::stdin().read_to_end(&mut raw)?;
            raw
        }
    };
    let uid = MailService::new(store.clone())
        .import_raw_message(&account, &args.mailbox, &raw)
        .await?;
    print_value(
        &serde_json::json!({"account_id":account.id,"mailbox":args.mailbox,"uid":uid}),
        json,
    )
}

async fn run_export_message(store: &Store, args: MessageExportArgs, json: bool) -> Result<()> {
    let message = store
        .message(&args.message_id)
        .await?
        .context("message not found")?;
    let account = store
        .account(Uuid::parse_str(&message.account_id)?)
        .await?
        .context("account not found")?;
    let raw = MailService::new(store.clone())
        .export_raw(&account, &message.mailbox, u32::try_from(message.uid)?)
        .await?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)
        .with_context(|| format!("could not create {}", args.output.display()))?;
    output.write_all(&raw)?;
    output.sync_all()?;
    print_value(
        &serde_json::json!({"message_id":args.message_id,"path":args.output,"bytes":raw.len()}),
        json,
    )
}

async fn run_folder_command(store: &Store, command: FolderCommand, json: bool) -> Result<()> {
    match command {
        FolderCommand::List(args) => {
            let account = store
                .account(args.account)
                .await?
                .context("account not found")?;
            print_value(
                &MailService::new(store.clone())
                    .list_folders(&account)
                    .await?,
                json,
            )
        }
        FolderCommand::Create(args) => {
            let account = store
                .account(args.account)
                .await?
                .context("account not found")?;
            MailService::new(store.clone())
                .create_folder(&account, &args.name)
                .await?;
            print_status(
                json,
                serde_json::json!({"status":"created","account_id":account.id,"name":args.name}),
                "Created folder",
            )
        }
        FolderCommand::Rename(args) => {
            let account = store
                .account(args.account)
                .await?
                .context("account not found")?;
            MailService::new(store.clone())
                .rename_folder(&account, &args.name, &args.new_name)
                .await?;
            print_status(
                json,
                serde_json::json!({"status":"renamed","account_id":account.id,"name":args.name,"new_name":args.new_name}),
                "Renamed folder",
            )
        }
        FolderCommand::Delete(args) => {
            require_confirmation(args.yes, "delete this remote folder")?;
            let account = store
                .account(args.account)
                .await?
                .context("account not found")?;
            MailService::new(store.clone())
                .delete_folder(&account, &args.name)
                .await?;
            print_status(
                json,
                serde_json::json!({"status":"deleted","account_id":account.id,"name":args.name}),
                "Deleted folder",
            )
        }
    }
}

async fn run_draft_command(store: &Store, command: DraftCommand, json: bool) -> Result<()> {
    let service = DraftService::new(store.clone(), MailService::new(store.clone()));
    match command {
        DraftCommand::List(args) => print_value(&service.list(args.account).await?, json),
        DraftCommand::Show(args) => print_value(
            &service
                .get(&args.draft_id.to_string())
                .await?
                .context("draft not found")?,
            json,
        ),
        DraftCommand::Create(args) => {
            let draft = service
                .create(ComposeMessage {
                    account_id: args.account,
                    to: args.to,
                    cc: args.cc,
                    bcc: args.bcc,
                    subject: args.subject,
                    body_text: body_or_stdin(args.body)?,
                    body_html: args.html_body,
                    in_reply_to: None,
                    references: None,
                    attachments: read_outbound_attachments(&args.attachments)?,
                })
                .await?;
            print_value(&draft, json)
        }
        DraftCommand::Edit(args) => {
            let current = service
                .get(&args.draft_id.to_string())
                .await?
                .context("draft not found")?;
            let compose = ComposeMessage {
                account_id: current.account_id,
                to: args.to.unwrap_or(current.compose.to),
                cc: args.cc.unwrap_or(current.compose.cc),
                bcc: args.bcc.unwrap_or(current.compose.bcc),
                subject: args.subject.unwrap_or(current.compose.subject),
                body_text: args.body.unwrap_or(current.compose.body_text),
                body_html: args.html_body.or(current.compose.body_html),
                in_reply_to: current.compose.in_reply_to,
                references: current.compose.references,
                attachments: current.compose.attachments,
            };
            print_value(
                &service
                    .update(&args.draft_id.to_string(), args.revision, compose)
                    .await?,
                json,
            )
        }
        DraftCommand::Delete(args) => {
            require_confirmation(args.yes, "delete this local draft")?;
            if !service.delete(&args.draft_id.to_string()).await? {
                bail!("draft not found");
            }
            print_status(
                json,
                serde_json::json!({"status":"deleted","draft_id":args.draft_id}),
                "Deleted draft",
            )
        }
        DraftCommand::Reply(args) => {
            let message = fetch_message(store, &args.message_id).await?;
            let account = store
                .account(Uuid::parse_str(&message.account_id)?)
                .await?
                .context("account not found")?;
            let compose = MailService::new(store.clone()).reply_draft(
                &account,
                &message,
                body_or_stdin(args.body)?,
            )?;
            print_value(&service.create(compose).await?, json)
        }
        DraftCommand::Forward(args) => {
            let message = fetch_message(store, &args.message_id).await?;
            let account = store
                .account(Uuid::parse_str(&message.account_id)?)
                .await?
                .context("account not found")?;
            let mut compose = MailService::new(store.clone()).forward_draft(
                &account,
                &message,
                body_or_stdin(args.body)?,
            );
            if args.attach_original {
                let raw = MailService::new(store.clone())
                    .export_raw(&account, &message.mailbox, u32::try_from(message.uid)?)
                    .await?;
                compose
                    .attachments
                    .push(dakia_core::mail::ComposeAttachment {
                        filename: format!("forwarded-{}.eml", message.id),
                        mime_type: "message/rfc822".into(),
                        content_base64: STANDARD.encode(raw),
                    });
            }
            print_value(&service.create(compose).await?, json)
        }
        DraftCommand::Send(args) => {
            let draft = service
                .get(&args.draft_id.to_string())
                .await?
                .context("draft not found")?;
            let account = store
                .account(draft.account_id)
                .await?
                .context("account not found")?;
            let response = service.send(&account, &args.draft_id.to_string()).await?;
            print_value(
                &serde_json::json!({"status":"sent","draft_id":args.draft_id,"response":response}),
                json,
            )
        }
    }
}

fn body_or_stdin(value: Option<String>) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None => {
            let mut body = String::new();
            io::stdin().read_to_string(&mut body)?;
            Ok(body)
        }
    }
}

async fn run_context_command(store: &Store, command: ContextCommand, json: bool) -> Result<()> {
    match command {
        ContextCommand::Get(args) => {
            let locator = context_locator(store, &args).await?;
            print_value(
                &store
                    .thread_context_get(locator, context_kind(args.kind))
                    .await?,
                json,
            )
        }
        ContextCommand::Set(args) => {
            let text = read_context_text(args.text)?;
            let context = if let Some(target) = source_context_target(&args.target)? {
                store
                    .thread_context_set_for_target(
                        &target,
                        context_kind(args.target.kind),
                        text,
                        args.revision,
                    )
                    .await?
            } else {
                store
                    .thread_context_set(
                        context_locator(store, &args.target).await?,
                        context_kind(args.target.kind),
                        text,
                        args.revision,
                    )
                    .await?
            };
            print_value(&context, json)
        }
        ContextCommand::Append(args) => {
            let text = read_context_text(args.text)?;
            let context = if let Some(target) = source_context_target(&args.target)? {
                store
                    .thread_context_append_for_target(
                        &target,
                        &text,
                        context_kind(args.target.kind),
                        args.revision,
                    )
                    .await?
            } else {
                store
                    .thread_context_append(
                        context_locator(store, &args.target).await?,
                        text,
                        context_kind(args.target.kind),
                        args.revision,
                    )
                    .await?
            };
            print_value(&context, json)
        }
        ContextCommand::Clear(args) => {
            let context = if let Some(target) = source_context_target(&args.target)? {
                store
                    .thread_context_clear_for_target(
                        &target,
                        context_kind(args.target.kind),
                        args.revision,
                    )
                    .await?
            } else {
                store
                    .thread_context_clear(
                        context_locator(store, &args.target).await?,
                        context_kind(args.target.kind),
                        args.revision,
                    )
                    .await?
            };
            print_value(&context, json)
        }
        ContextCommand::History(args) => {
            let locator = context_locator(store, &args).await?;
            print_value(
                &store
                    .thread_context_history(locator, context_kind(args.kind))
                    .await?,
                json,
            )
        }
        ContextCommand::Restore(args) => {
            let context = if let Some(target) = source_context_target(&args.target)? {
                store
                    .thread_context_restore_for_target(
                        &target,
                        context_kind(args.target.kind),
                        args.revision_id,
                        args.revision,
                    )
                    .await?
            } else {
                store
                    .thread_context_restore(
                        context_locator(store, &args.target).await?,
                        context_kind(args.target.kind),
                        args.revision_id,
                        args.revision,
                    )
                    .await?
            };
            print_value(&context, json)
        }
    }
}

fn source_context_target(target: &ContextTargetArgs) -> Result<Option<ConversationTarget>> {
    if target.conversation_id.is_some() {
        return Ok(None);
    }
    let thread = &target.target;
    if thread.message_id.is_none() && thread.rfc_message_id.is_none() && thread.thread_id.is_none()
    {
        bail!("context requires --message-id, --rfc-message-id, or --thread-id");
    }
    Ok(Some(conversation_target(thread)))
}

async fn context_locator(
    store: &Store,
    target: &ContextTargetArgs,
) -> Result<dakia_core::ThreadContextLocator> {
    if let Some(conversation_id) = target.conversation_id {
        return Ok(dakia_core::ThreadContextLocator {
            account_id: target.target.account,
            conversation_id,
        });
    }
    let target = &target.target;
    if target.message_id.is_none() && target.rfc_message_id.is_none() && target.thread_id.is_none()
    {
        bail!("context requires --message-id, --rfc-message-id, or --thread-id");
    }
    store
        .thread_context_conversation_for_target(&conversation_target(target))
        .await?
        .map(|conversation| conversation.locator)
        .context("conversation not found")
}

fn context_kind(kind: ContextKind) -> ThreadContextKind {
    match kind {
        ContextKind::Notes => ThreadContextKind::Notes,
        ContextKind::AgentMemory => ThreadContextKind::AgentMemory,
    }
}

fn read_context_text(value: Option<String>) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None if io::stdin().is_terminal() => {
            bail!("context text is required; pass --text or pipe it through standard input")
        }
        None => {
            let mut value = String::new();
            io::stdin().read_to_string(&mut value)?;
            Ok(value)
        }
    }
}

async fn run_watch(store: &Store, args: WatchArgs, json: bool) -> Result<()> {
    let accounts = store
        .accounts()
        .await?
        .into_iter()
        .filter(|account| {
            account.enabled && (args.account.is_empty() || args.account.contains(&account.id))
        })
        .collect::<Vec<_>>();
    if !args.account.is_empty() && accounts.len() != args.account.len() {
        bail!("one or more requested accounts were not found or are disabled");
    }
    if accounts.is_empty() {
        return Ok(());
    }
    let (cancel_tx, _) = tokio::sync::watch::channel(false);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
    let mut workers = tokio::task::JoinSet::new();
    for account in accounts {
        let service = MailService::new(store.clone());
        let draft_store = store.clone();
        let events = event_tx.clone();
        let mut cancel = cancel_tx.subscribe();
        let limit = args.limit;
        workers.spawn(async move {
            loop {
                match service
                    .realtime_inbox_cycle(&account, limit, &mut cancel)
                    .await
                {
                    Ok(cycle) if cycle.cancelled || *cancel.borrow() => {
                        return Ok::<(), anyhow::Error>(())
                    }
                    Ok(cycle) => {
                        let _ = DraftService::new(draft_store.clone(), service.clone())
                            .reconcile_pending(&account, 20)
                            .await;
                        for message in cycle.new_messages {
                            if events
                                .send(Ok(
                                    serde_json::json!({"account_id":account.id,"message":message}),
                                ))
                                .await
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    Err(error) => {
                        if events
                            .send(Err(anyhow::anyhow!(
                                "watch failed for {}: {error}",
                                account.email
                            )))
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }
    drop(event_tx);
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = cancel_tx.send(true);
                workers.abort_all();
                while workers.join_next().await.is_some() {}
                return Ok(());
            }
            event = event_rx.recv() => match event {
                Some(Ok(value)) if json => print_value(&value, true)?,
                Some(Ok(value)) => println!("new mail: {}", value["message"]["subject"].as_str().unwrap_or("(no subject)")),
                Some(Err(error)) => eprintln!("Warning: {error}"),
                None => return Ok(()),
            }
        }
    }
}

async fn run_classification(store: &Store, args: ClassifyArgs) -> Result<usize> {
    let owner = Uuid::new_v4().to_string();
    let result = run_owned_classification(store, args, &owner).await;
    let release = store.release_classification_revision(&owner).await;
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(classified), Ok(())) => Ok(classified),
    }
}

async fn run_owned_classification(store: &Store, args: ClassifyArgs, owner: &str) -> Result<usize> {
    let mut classifier = LocalEmailClassifier::from_dir(args.model_dir)?;
    store
        .claim_classification_revision(owner, classifier.revision())
        .await?;
    let messages = if args.all {
        store.messages_for_model_reclassification().await?
    } else {
        Vec::new()
    };
    let mut classified = 0;
    if args.all {
        for batch in messages.chunks(CLASSIFICATION_BATCH_SIZE) {
            classified += classify_batch(store, &mut classifier, batch, owner).await?;
        }
    } else {
        loop {
            let batch = store
                .messages_for_model_classification_batch(CLASSIFICATION_BATCH_SIZE)
                .await?;
            if batch.is_empty() {
                break;
            }
            classified += classify_batch(store, &mut classifier, &batch, owner).await?;
        }
    }
    Ok(classified)
}

async fn classify_batch(
    store: &Store,
    classifier: &mut LocalEmailClassifier,
    messages: &[dakia_core::MailSummary],
    owner: &str,
) -> Result<usize> {
    let model_revision = classifier.revision().to_owned();
    store
        .claim_classification_revision(owner, &model_revision)
        .await?;
    let ids: Vec<String> = messages.iter().map(|message| message.id.clone()).collect();
    let known_correspondence = store.messages_from_known_correspondents(messages).await?;
    let inputs: Vec<EmailClassificationInput> = messages
        .iter()
        .map(|message| {
            let body = if message.body_text.trim().is_empty() {
                &message.snippet
            } else {
                &message.body_text
            };
            EmailClassificationInput::new(
                message.from_name.as_deref(),
                &message.from_address,
                &message.subject,
                body,
                &message.classification_signals,
            )
            .with_known_correspondence(known_correspondence.contains(&message.id))
        })
        .collect();
    let classifications = classifier.classify(&inputs)?;
    let updates = classification_updates(ids, classifications)?
        .into_iter()
        .zip(messages)
        .map(|((_id, category, confidence), message)| {
            ModelClassificationUpdate::from_message(
                message,
                category,
                confidence,
                known_correspondence.contains(&message.id),
                owner,
                &model_revision,
            )
        })
        .collect::<Vec<_>>();
    let count = updates.len();
    let applied = store.apply_model_classifications(&updates).await?;
    validate_classification_apply_count(count, applied)
}

fn validate_classification_apply_count(attempted: usize, applied: usize) -> Result<usize> {
    if attempted != applied {
        bail!(
            "classification batch became stale ({applied}/{attempted} results applied); rerun after the active classifier finishes"
        );
    }
    Ok(applied)
}

fn classification_updates(
    ids: Vec<String>,
    classifications: Vec<dakia_core::classification::ModelClassification>,
) -> Result<Vec<(String, String, Option<f64>)>> {
    if ids.len() != classifications.len() {
        bail!(
            "classifier returned {} results for {} messages",
            classifications.len(),
            ids.len()
        );
    }
    Ok(ids
        .into_iter()
        .zip(classifications)
        .map(|(id, result)| (id, result.category, result.confidence))
        .collect())
}

fn default_data_dir() -> PathBuf {
    // This is the Tauri bundle identifier used by the current desktop app,
    // including `tauri dev`.
    ProjectDirs::from("dev", "dakia", "mail")
        .expect("platform has no application data directory")
        .data_local_dir()
        .to_owned()
}

async fn search_remote(store: &Store, query: &SearchQuery) -> Result<Vec<dakia_core::MailSummary>> {
    if query.text.trim().is_empty() {
        bail!("remote search requires a non-empty query");
    }
    let accounts = store.accounts().await?;
    let accounts = accounts
        .into_iter()
        .filter(|account| query.account_ids.is_empty() || query.account_ids.contains(&account.id))
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    let per_account_limit = query.limit.unwrap_or(100).min(500) as usize;
    let search_store = store.clone();
    let text = query.text.clone();
    let mailbox = query.mailbox.clone();
    let searches = run_bounded_ordered(accounts, REMOTE_SEARCH_CONCURRENCY, move |account| {
        let store = search_store.clone();
        let text = text.clone();
        let mailbox = mailbox.clone();
        async move {
            MailService::new(store)
                .search_remote(&account, &text, mailbox.as_deref(), per_account_limit)
                .await
                .with_context(|| format!("remote search failed for {}", account.email))
        }
    })
    .await;
    // Results are restored to Store::accounts order before filtering and the
    // existing stable timestamp sort. If several accounts fail, report the
    // first failure in that same deterministic order.
    for hits in searches {
        let hits = hits?;
        results.extend(hits.into_iter().filter(|message| {
            (!query.unread_only || !message.is_read)
                && query
                    .from
                    .as_deref()
                    .map(|from| message.from_address.contains(from))
                    .unwrap_or(true)
        }));
    }
    results.sort_by_key(|result| std::cmp::Reverse(result.received_at));
    results.truncate(per_account_limit);
    Ok(results)
}

async fn run_bounded_ordered<T, U, E, F, Fut>(
    items: Vec<T>,
    max_in_flight: usize,
    operation: F,
) -> Vec<Result<U, E>>
where
    T: Send + 'static,
    U: Send + 'static,
    E: Send + 'static,
    F: Fn(T) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<U, E>> + Send + 'static,
{
    assert!(max_in_flight > 0, "bounded work requires a non-zero limit");
    let expected = items.len();
    let mut pending = items.into_iter().enumerate();
    let mut active = tokio::task::JoinSet::new();
    let mut completed = Vec::with_capacity(expected);

    loop {
        while active.len() < max_in_flight {
            let Some((index, item)) = pending.next() else {
                break;
            };
            let operation = operation.clone();
            active.spawn(async move { (index, operation(item).await) });
        }
        let Some(joined) = active.join_next().await else {
            break;
        };
        completed.push(joined.expect("bounded task must not panic"));
    }

    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, result)| result).collect()
}

async fn search_local_and_remote(
    store: &Store,
    query: &SearchQuery,
) -> Result<Vec<dakia_core::MailSummary>> {
    let mut results = store.search(query).await?;
    let mut known = results
        .iter()
        .map(|message| message.id.clone())
        .collect::<HashSet<_>>();
    for message in search_remote(store, query).await? {
        if known.insert(message.id.clone()) {
            results.push(message);
        }
    }
    results.sort_by_key(|result| std::cmp::Reverse(result.received_at));
    results.truncate(query.limit.unwrap_or(100).min(500) as usize);
    Ok(results)
}

async fn fetch_message(store: &Store, message_id: &str) -> Result<dakia_core::MailSummary> {
    let summary = store
        .message(message_id)
        .await?
        .context("message not found")?;
    let account_id =
        Uuid::parse_str(&summary.account_id).context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    MailService::new(store.clone())
        .fetch_message(
            &account,
            // `fetch_message` resolves the stored local mailbox through the
            // shared catalogue state. Passing a provider name here bypasses
            // that state for Gmail's special folders (for example Sent).
            &summary.mailbox,
            u32::try_from(summary.uid).context("stored message has an invalid UID")?,
        )
        .await
}

async fn apply_mailbox_action(
    store: &Store,
    message_ids: &[String],
    action: MailboxAction,
    json: bool,
) -> Result<()> {
    let mut completed = Vec::with_capacity(message_ids.len());
    let mut errors = Vec::new();
    for message_id in message_ids {
        let result = async {
            let summary = store
                .message(message_id)
                .await?
                .with_context(|| format!("message not found: {message_id}"))?;
            let account_id = Uuid::parse_str(&summary.account_id)
                .with_context(|| format!("invalid account ID for message {message_id}"))?;
            let account = store
                .account(account_id)
                .await?
                .with_context(|| format!("account not found for message {message_id}"))?;
            MailService::new(store.clone())
                .apply_action(
                    &account,
                    &summary.mailbox,
                    u32::try_from(summary.uid).context("stored message has an invalid UID")?,
                    action,
                )
                .await
                .with_context(|| format!("mailbox action failed for {message_id}"))
        }
        .await;
        match result {
            Ok(_) => completed.push(message_id),
            Err(error) => errors.push(serde_json::json!({
                "message_id": message_id,
                "message": error.to_string(),
            })),
        }
    }
    if !errors.is_empty() {
        return Err(partial_failure(
            "mailbox action only completed for some messages",
            serde_json::json!({"action": action_name(action), "message_ids": completed}),
            errors,
        ));
    }
    if json {
        print_value(
            &serde_json::json!({"action": action_name(action), "message_ids": completed}),
            true,
        )
    } else {
        println!(
            "{} {} message{}",
            action_name(action),
            completed.len(),
            if completed.len() == 1 { "" } else { "s" }
        );
        Ok(())
    }
}

fn partial_failure(
    message: impl Into<String>,
    completed: serde_json::Value,
    errors: Vec<serde_json::Value>,
) -> anyhow::Error {
    anyhow::Error::new(PartialFailure {
        message: message.into(),
        details: serde_json::json!({"completed":completed,"errors":errors}),
    })
}

async fn download_attachment(
    store: &Store,
    args: DownloadAttachmentArgs,
    json: bool,
) -> Result<()> {
    let summary = store
        .message(&args.message_id)
        .await?
        .context("message not found")?;
    let account_id =
        Uuid::parse_str(&summary.account_id).context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    let attachment = MailService::new(store.clone())
        .fetch_attachment(
            &account,
            &summary.mailbox,
            u32::try_from(summary.uid).context("stored message has an invalid UID")?,
            &args.attachment_id,
        )
        .await
        .context("attachment not found")?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)
        .with_context(|| format!("could not create {}", args.output.display()))?;
    output.write_all(&attachment.bytes)?;
    output.sync_all()?;
    if json {
        print_value(
            &serde_json::json!({
                "attachment_id": attachment.attachment.id,
                "path": args.output,
                "bytes": attachment.bytes.len(),
            }),
            true,
        )
    } else {
        println!("saved {}", args.output.display());
        Ok(())
    }
}

fn read_outbound_attachments(
    paths: &[PathBuf],
) -> Result<Vec<dakia_core::mail::ComposeAttachment>> {
    const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
    const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
    const MAX_ATTACHMENTS: usize = 50;
    if paths.len() > MAX_ATTACHMENTS {
        bail!("a message can include at most {MAX_ATTACHMENTS} attachments");
    }
    let mut total = 0_u64;
    paths
        .iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("could not inspect {}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("attachments must be regular files: {}", path.display());
            }
            if metadata.len() > MAX_ATTACHMENT_BYTES {
                bail!(
                    "{} exceeds the {} MiB attachment limit",
                    path.display(),
                    MAX_ATTACHMENT_BYTES / 1024 / 1024
                );
            }
            total += metadata.len();
            if total > MAX_TOTAL_ATTACHMENT_BYTES {
                bail!(
                    "attachments exceed the {} MiB total limit",
                    MAX_TOTAL_ATTACHMENT_BYTES / 1024 / 1024
                );
            }
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .context("attachment filename is invalid")?
                .to_owned();
            Ok(dakia_core::mail::ComposeAttachment {
                mime_type: mime_type_for_filename(&filename).into(),
                filename,
                content_base64: STANDARD.encode(fs::read(path)?),
            })
        })
        .collect()
}

fn mime_type_for_filename(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => "text/csv",
        "gif" => "image/gif",
        "jpeg" | "jpg" => "image/jpeg",
        "json" => "application/json",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "webp" => "image/webp",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn require_confirmation(confirmed: bool, action: &str) -> Result<()> {
    if confirmed {
        Ok(())
    } else {
        bail!("refusing to {action} without --yes")
    }
}

fn action_name(action: MailboxAction) -> &'static str {
    match action {
        MailboxAction::Archive => "archived",
        MailboxAction::Spam => "marked as spam",
        MailboxAction::NotSpam => "marked as not spam",
        MailboxAction::Trash => "moved to Trash",
        MailboxAction::Delete => "permanently deleted",
    }
}

fn ai_from_env() -> Result<AiService> {
    let kind = std::env::var("DAKIA_AI_PROVIDER").unwrap_or_else(|_| "ollama".into());
    let model = std::env::var("DAKIA_AI_MODEL").unwrap_or_else(|_| "qwen2.5:1.5b".into());
    let provider = match kind.as_str() {
        "ollama" => AiProvider::Ollama {
            base_url: Url::parse(
                &std::env::var("DAKIA_AI_BASE_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:11434/".into()),
            )?,
            model,
        },
        "openai" => AiProvider::OpenAiCompatible {
            base_url: Url::parse(
                &std::env::var("DAKIA_AI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1/".into()),
            )?,
            model,
        },
        "local" => AiProvider::LocalCommand {
            executable: std::env::var_os("DAKIA_AI_EXECUTABLE")
                .context("DAKIA_AI_EXECUTABLE is required")?
                .into(),
            model_path: std::env::var_os("DAKIA_AI_MODEL_PATH")
                .context("DAKIA_AI_MODEL_PATH is required")?
                .into(),
            extra_args: Vec::new(),
        },
        _ => bail!("DAKIA_AI_PROVIDER must be ollama, openai, or local"),
    };
    Ok(AiService::new(AiConfig {
        provider,
        api_key: std::env::var("DAKIA_AI_API_KEY")
            .ok()
            .map(SecretString::from),
    }))
}

fn print_value(value: &impl serde::Serialize, _json: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use dakia_core::classification::ModelClassification;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[test]
    fn ai_cli_keeps_summarization_but_rejects_llm_translation() {
        assert!(Cli::try_parse_from(["dakia", "ai", "summarize", "message-1"]).is_ok());
        let error = Cli::try_parse_from(["dakia", "ai", "translate", "message-1"])
            .err()
            .expect("translate must not be an AI subcommand");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn plugin_commands_are_not_part_of_the_public_cli() {
        let error = Cli::try_parse_from(["dakia", "plugin", "list"])
            .err()
            .expect("plugin support must remain unavailable");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn classification_updates_rejects_short_classifier_output() {
        let error = classification_updates(
            vec!["message-1".into(), "message-2".into()],
            vec![ModelClassification {
                category: "work".into(),
                confidence: Some(0.9),
            }],
        )
        .expect_err("short classifier output must not partially update messages");

        assert_eq!(
            error.to_string(),
            "classifier returned 1 results for 2 messages"
        );
    }

    #[test]
    fn classification_updates_rejects_long_classifier_output() {
        let error = classification_updates(
            vec!["message-1".into()],
            vec![
                ModelClassification {
                    category: "work".into(),
                    confidence: Some(0.9),
                },
                ModelClassification {
                    category: "personal".into(),
                    confidence: Some(0.8),
                },
            ],
        )
        .expect_err("long classifier output must not apply any messages");

        assert_eq!(
            error.to_string(),
            "classifier returned 2 results for 1 messages"
        );
    }

    #[test]
    fn stale_classification_apply_stops_instead_of_looping() {
        assert_eq!(validate_classification_apply_count(2, 2).unwrap(), 2);
        assert_eq!(
            validate_classification_apply_count(2, 0)
                .unwrap_err()
                .to_string(),
            "classification batch became stale (0/2 results applied); rerun after the active classifier finishes"
        );
    }

    #[tokio::test]
    async fn bounded_work_caps_concurrency_and_restores_input_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let results = run_bounded_ordered((0..12).collect(), 3, {
            let active = active.clone();
            let peak = peak.clone();
            move |index| {
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis((12 - index) as u64)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, ()>(index)
                }
            }
        })
        .await;

        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            (0..12).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn cancelling_bounded_work_aborts_its_in_flight_tasks() {
        struct ActiveGuard(Arc<AtomicUsize>);
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_bounded_ordered((0..20).collect(), 2, {
            let active = active.clone();
            move |_| {
                let active = active.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = ActiveGuard(active);
                    std::future::pending::<Result<(), ()>>().await
                }
            }
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(active.load(Ordering::SeqCst), 2);

        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bounded_work_reports_failures_in_input_order() {
        let results = run_bounded_ordered((0..4).collect(), 4, |index| async move {
            tokio::time::sleep(Duration::from_millis((4 - index) as u64)).await;
            if matches!(index, 1 | 3) {
                Err(index)
            } else {
                Ok(index)
            }
        })
        .await;

        assert_eq!(results, vec![Ok(0), Err(1), Ok(2), Err(3)]);
        assert_eq!(
            results.into_iter().find_map(Result::err),
            Some(1),
            "the first account-order failure is the deterministic CLI error"
        );
    }
}
