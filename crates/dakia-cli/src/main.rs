use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, ComposeMessage, EmailClassificationInput, LocalEmailClassifier,
    MailService, MailboxAction, ModelClassificationUpdate, SearchQuery, Store,
};
use directories::ProjectDirs;
use secrecy::SecretString;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    io::{self, Read},
    path::PathBuf,
};
use url::Url;
use uuid::Uuid;

const REMOTE_SEARCH_CONCURRENCY: usize = 4;
const CLASSIFICATION_BATCH_SIZE: usize = 64;

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
    Ai {
        #[command(subcommand)]
        command: AiCommand,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    List,
}

#[derive(Args)]
struct SyncArgs {
    #[arg(long)]
    account: Option<Uuid>,
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
    query: String,
    #[arg(long)]
    account: Vec<Uuid>,
    #[arg(long)]
    mailbox: Option<String>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    unread: bool,
    #[arg(long, default_value_t = 50)]
    limit: u32,
    /// Query the provider as well as the local catalogue. Remote results are
    /// saved as metadata so they are immediately available in the desktop app.
    #[arg(long)]
    remote: bool,
}

#[derive(Args)]
struct ShowArgs {
    /// The stable Dakia message ID returned by `search`.
    message_id: String,
    /// The mailbox UIDVALIDITY returned with the message by `dakia --json search`.
    /// Dakia refuses to fetch if the message has moved to a newer mailbox generation.
    #[arg(long)]
    expected_uid_validity: Option<u32>,
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
    /// The mailbox UIDVALIDITY returned with the message by `dakia --json search`.
    /// Dakia refuses to download when the message has moved to a newer mailbox generation.
    #[arg(long)]
    expected_uid_validity: Option<u32>,
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
    /// UIDVALIDITY values from `dakia --json search`, in the same order as
    /// message_id. Dakia refuses an action if a mailbox generation changed.
    #[arg(long)]
    expected_uid_validity: Vec<u32>,
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
enum AiCommand {
    Summarize(AiMessagesArgs),
}

#[derive(Args)]
struct AiMessagesArgs {
    #[arg(required = true)]
    message_id: Vec<String>,
    /// UIDVALIDITY values from `dakia --json search`, in the same order as
    /// message_id. Required before email content can be sent to an AI provider.
    #[arg(long)]
    expected_uid_validity: Vec<u32>,
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    let store = Store::open(data_dir.join("dakia.db")).await?;
    match cli.command {
        Command::Account { command } => match command {
            AccountCommand::List => print_value(&store.accounts().await?, cli.json)?,
        },
        Command::Sync(args) => {
            let mail = MailService::new(store.clone());
            let accounts = store
                .accounts()
                .await?
                .into_iter()
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
                } else {
                    mail.sync_all(&account, args.limit)
                        .await
                        .with_context(|| format!("sync failed for {}", account.email))?
                };
                if cli.json {
                    print_value(
                        &if args.refresh_gmail_categories {
                            serde_json::json!({"account_id":account.id,"refreshed_category_labels":count})
                        } else {
                            serde_json::json!({"account_id":account.id,"synced":count})
                        },
                        true,
                    )?;
                } else {
                    let action = if args.refresh_gmail_categories {
                        "refreshed Gmail category labels for"
                    } else {
                        "synced"
                    };
                    println!("{}: {action} {count} messages", account.email);
                }
            }
        }
        Command::Classify(args) => {
            let classified = run_classification(&store, args).await?;
            if cli.json {
                println!("{}", serde_json::json!({"classified" : classified}));
            } else {
                println!("classified {classified} messages with the local ONNX model");
            }
        }
        Command::Search(args) => {
            let query = SearchQuery {
                text: args.query,
                account_ids: args.account,
                mailbox: args.mailbox,
                from: args.from,
                unread_only: args.unread,
                read_only: false,
                flagged_only: false,
                unflagged_only: false,
                category: None,
                limit: Some(args.limit),
                cursor: None,
            };
            let results = if args.remote {
                search_local_and_remote(&store, &query).await?
            } else {
                store.search(&query).await?
            };
            if cli.json {
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
            let message =
                fetch_message(&store, &args.message_id, args.expected_uid_validity).await?;
            print_value(&message, cli.json)?;
        }
        Command::Attachment { command } => match command {
            AttachmentCommand::List(args) => {
                let message =
                    fetch_message(&store, &args.message_id, args.expected_uid_validity).await?;
                let attachments = message
                    .attachments
                    .iter()
                    .map(|item| &item.attachment)
                    .collect::<Vec<_>>();
                print_value(&attachments, cli.json)?;
            }
            AttachmentCommand::Download(args) => {
                download_attachment(&store, args, cli.json).await?;
            }
        },
        Command::Archive(args) => {
            apply_mailbox_action(
                &store,
                &message_receipts(&args.message_id, &args.expected_uid_validity)?,
                MailboxAction::Archive,
                cli.json,
            )
            .await?
        }
        Command::Spam(args) => {
            apply_mailbox_action(
                &store,
                &message_receipts(&args.message_id, &args.expected_uid_validity)?,
                MailboxAction::Spam,
                cli.json,
            )
            .await?
        }
        Command::Trash(args) => {
            require_confirmation(args.yes, "trash")?;
            apply_mailbox_action(
                &store,
                &message_receipts(
                    &args.messages.message_id,
                    &args.messages.expected_uid_validity,
                )?,
                MailboxAction::Trash,
                cli.json,
            )
            .await?
        }
        Command::Delete(args) => {
            require_confirmation(args.yes, "permanently delete")?;
            apply_mailbox_action(
                &store,
                &message_receipts(
                    &args.messages.message_id,
                    &args.messages.expected_uid_validity,
                )?,
                MailboxAction::Delete,
                cli.json,
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
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"status":"sent","response":response})
                );
            } else {
                println!("Sent: {response}");
            }
        }
        Command::Ai { command } => match command {
            AiCommand::Summarize(args) => {
                let messages = ai_messages_at_receipts(
                    &store,
                    &message_receipts(&args.message_id, &args.expected_uid_validity)?,
                )
                .await?;
                let ai = ai_from_env()?;
                println!("{}", ai.summarize(&messages).await?);
            }
        },
    }
    Ok(())
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

async fn fetch_message(
    store: &Store,
    message_id: &str,
    expected_uid_validity: Option<u32>,
) -> Result<dakia_core::MailSummary> {
    let expected_uid_validity = expected_uid_validity
        .context("reading a message requires --expected-uid-validity from dakia --json search")?;
    let receipt = message_receipts(
        &[message_id.to_owned()],
        std::slice::from_ref(&expected_uid_validity),
    )?
    .into_iter()
    .next()
    .expect("one message receipt");
    let summary = message_at_receipt(store, &receipt).await?;
    let account_id =
        Uuid::parse_str(&summary.account_id).context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    let fetched = MailService::new(store.clone())
        .fetch_message_at_epoch(
            &account,
            // `fetch_message` resolves the stored local mailbox through the
            // shared catalogue state. Passing a provider name here bypasses
            // that state for Gmail's special folders (for example Sent).
            &summary.mailbox,
            u32::try_from(summary.uid).context("stored message has an invalid UID")?,
            receipt.expected_uid_validity,
        )
        .await?;
    let current = message_at_receipt(store, &receipt).await?;
    if !same_message_target(&summary, &current) {
        bail!("message changed while reading it; no content was displayed");
    }
    Ok(fetched)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageReceipt {
    message_id: String,
    expected_uid_validity: i64,
}

fn message_receipts(
    message_ids: &[String],
    expected_uid_validities: &[u32],
) -> Result<Vec<MessageReceipt>> {
    if message_ids.len() != expected_uid_validities.len() {
        bail!("provide --expected-uid-validity once for each message_id, in the same order");
    }
    message_ids
        .iter()
        .zip(expected_uid_validities)
        .map(|(message_id, uid_validity)| {
            if *uid_validity == 0 {
                bail!("--expected-uid-validity must be greater than zero");
            }
            Ok(MessageReceipt {
                message_id: message_id.clone(),
                expected_uid_validity: i64::from(*uid_validity),
            })
        })
        .collect()
}

async fn message_at_receipt(
    store: &Store,
    receipt: &MessageReceipt,
) -> Result<dakia_core::MailSummary> {
    let summary = store
        .message(&receipt.message_id)
        .await?
        .with_context(|| format!("message not found: {}", receipt.message_id))?;
    let current_uid_validity = summary
        .mailbox_uid_validity
        .context("message mailbox identity is unavailable; rerun search and try again")?;
    if current_uid_validity != receipt.expected_uid_validity {
        bail!("message changed since it was selected; rerun search and try again");
    }
    Ok(summary)
}

fn same_message_target(
    captured: &dakia_core::MailSummary,
    current: &dakia_core::MailSummary,
) -> bool {
    captured.id == current.id
        && captured.account_id == current.account_id
        && captured.mailbox == current.mailbox
        && captured.uid == current.uid
        && captured.mailbox_uid_validity == current.mailbox_uid_validity
}

async fn ai_messages_at_receipts(
    store: &Store,
    receipts: &[MessageReceipt],
) -> Result<Vec<dakia_core::MailSummary>> {
    // This read happens immediately before constructing the provider request.
    // Stable local IDs alone may refer to a message that replaced an older IMAP
    // UIDVALIDITY generation, so do not release any content without a receipt.
    let mut messages = Vec::with_capacity(receipts.len());
    for receipt in receipts {
        messages.push(message_at_receipt(store, receipt).await?);
    }
    Ok(messages)
}

async fn apply_mailbox_action(
    store: &Store,
    receipts: &[MessageReceipt],
    action: MailboxAction,
    json: bool,
) -> Result<()> {
    let mut completed = Vec::with_capacity(receipts.len());
    for receipt_target in receipts {
        let summary = message_at_receipt(store, receipt_target).await?;
        let message_id = &receipt_target.message_id;
        let account_id = Uuid::parse_str(&summary.account_id)
            .with_context(|| format!("invalid account ID for message {message_id}"))?;
        let account = store
            .account(account_id)
            .await?
            .with_context(|| format!("account not found for message {message_id}"))?;
        let service = MailService::new(store.clone());
        let remote_receipt = service
            .apply_action_at_epoch_with_receipt(
                &account,
                &summary.mailbox,
                u32::try_from(summary.uid).context("stored message has an invalid UID")?,
                action,
                receipt_target.expected_uid_validity,
            )
            .await
            .with_context(|| format!("mailbox action failed for {message_id}"))?;
        let destination = mailbox_action_destination(action).unwrap_or_default();
        let publication = store
            .move_message_at_epoch(&summary, destination, &remote_receipt)
            .await?;
        if !publication.source_published {
            bail!("message changed while the mailbox action completed; sync before trying another action");
        }
        if mailbox_action_destination(action).is_some() && !publication.destination_mapped {
            service.sync_all(&account, 250).await.context(
                "mailbox action completed, but Dakia could not reconcile its destination",
            )?;
        }
        completed.push(message_id.clone());
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

async fn download_attachment(
    store: &Store,
    args: DownloadAttachmentArgs,
    json: bool,
) -> Result<()> {
    let expected_uid_validity = args
        .expected_uid_validity
        .context("attachment download requires --expected-uid-validity from dakia --json search")?;
    let receipt = message_receipts(
        std::slice::from_ref(&args.message_id),
        std::slice::from_ref(&expected_uid_validity),
    )?
    .into_iter()
    .next()
    .expect("one attachment receipt");
    let summary = message_at_receipt(store, &receipt).await?;
    let account_id =
        Uuid::parse_str(&summary.account_id).context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    let attachment = MailService::new(store.clone())
        .fetch_attachment_at_epoch(
            &account,
            &summary.mailbox,
            u32::try_from(summary.uid).context("stored message has an invalid UID")?,
            &args.attachment_id,
            receipt.expected_uid_validity,
        )
        .await
        .context("attachment not found")?;
    // The provider request above may take long enough for another process to
    // replace this mailbox generation. Never create a user-visible file for a
    // stale attachment response.
    let current = message_at_receipt(store, &receipt).await?;
    if !same_message_target(&summary, &current) {
        bail!("message changed while downloading the attachment; no file was created");
    }
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
    use chrono::Utc;
    use clap::error::ErrorKind;
    use dakia_core::{classification::ModelClassification, provider, AccountDraft, MailSummary};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    async fn store_with_reused_message_generation() -> (Store, String) {
        let store = Store::in_memory().await.unwrap();
        let account = AccountDraft {
            email: "receipt-test@example.test".into(),
            display_name: "Receipt test".into(),
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
        .into_account(provider::by_id("fastmail").unwrap());
        store.save_account(&account).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 78, 1, true)
            .await
            .unwrap();

        let id = "same-local-id-after-uidvalidity-rollover".to_owned();
        let message = MailSummary {
            id: id.clone(),
            account_id: account.id.to_string(),
            mailbox: "INBOX".into(),
            uid: 42,
            mailbox_uid_validity: Some(78),
            message_id: Some("replacement@example.test".into()),
            gmail_message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: "receipt-test-thread".into(),
            subject: "replacement message".into(),
            from_name: Some("Receipt test".into()),
            from_address: "sender@example.test".into(),
            to_addresses: "receipt-test@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: "replacement".into(),
            body_text: "replacement body".into(),
            body_html: None,
            content_state: "complete".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: false,
            is_flagged: false,
            has_attachments: true,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: Vec::new(),
        };
        store.upsert_messages(&[message]).await.unwrap();
        (store, id)
    }

    fn old_generation_receipt(message_id: &str) -> Vec<MessageReceipt> {
        message_receipts(&[message_id.to_owned()], &[77]).unwrap()
    }

    #[test]
    fn ai_cli_keeps_summarization_but_rejects_llm_translation() {
        assert!(Cli::try_parse_from(["dakia", "ai", "summarize", "message-1"]).is_ok());
        let error = Cli::try_parse_from(["dakia", "ai", "translate", "message-1"])
            .err()
            .expect("translate must not be an AI subcommand");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn message_receipts_require_one_positive_epoch_per_message() {
        let ids = vec!["message-1".to_owned(), "message-2".to_owned()];
        assert_eq!(
            message_receipts(&ids, &[77]).unwrap_err().to_string(),
            "provide --expected-uid-validity once for each message_id, in the same order"
        );
        assert_eq!(
            message_receipts(&ids[..1], &[0]).unwrap_err().to_string(),
            "--expected-uid-validity must be greater than zero"
        );
    }

    #[tokio::test]
    async fn stale_cli_action_receipt_does_not_mutate_a_reused_message() {
        let (store, id) = store_with_reused_message_generation().await;
        let error = apply_mailbox_action(
            &store,
            &old_generation_receipt(&id),
            MailboxAction::Archive,
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "message changed since it was selected; rerun search and try again"
        );
        let current = store.message(&id).await.unwrap().unwrap();
        assert_eq!(current.mailbox, "INBOX");
        assert_eq!(current.mailbox_uid_validity, Some(78));
    }

    #[tokio::test]
    async fn stale_cli_read_receipt_does_not_fetch_a_reused_message() {
        let (store, id) = store_with_reused_message_generation().await;
        let error = fetch_message(&store, &id, Some(77)).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "message changed since it was selected; rerun search and try again"
        );
    }

    #[tokio::test]
    async fn missing_cli_read_receipt_fails_before_finding_a_message() {
        let store = Store::in_memory().await.unwrap();
        let error = fetch_message(&store, "message-id", None).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "reading a message requires --expected-uid-validity from dakia --json search"
        );
    }

    #[tokio::test]
    async fn stale_cli_attachment_receipt_creates_no_file_or_provider_request() {
        let (store, id) = store_with_reused_message_generation().await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("must-not-exist.bin");
        let error = download_attachment(
            &store,
            DownloadAttachmentArgs {
                message_id: id,
                expected_uid_validity: Some(77),
                attachment_id: "old-attachment".into(),
                output: output.clone(),
            },
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "message changed since it was selected; rerun search and try again"
        );
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn missing_cli_attachment_receipt_fails_before_creating_a_file() {
        let store = Store::in_memory().await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("must-not-exist.bin");
        let error = download_attachment(
            &store,
            DownloadAttachmentArgs {
                message_id: "message-id".into(),
                expected_uid_validity: None,
                attachment_id: "attachment-id".into(),
                output: output.clone(),
            },
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "attachment download requires --expected-uid-validity from dakia --json search"
        );
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn stale_cli_ai_receipt_releases_no_replaced_message_content() {
        let (store, id) = store_with_reused_message_generation().await;
        let error = ai_messages_at_receipts(&store, &old_generation_receipt(&id))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "message changed since it was selected; rerun search and try again"
        );
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
