use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, ComposeMessage, EmailClassificationInput, LocalEmailClassifier,
    MailService, MailboxAction, ModelClassificationUpdate, PreparedOutgoingMessage, SearchQuery,
    SendOutcome, SentCopyOutcome, SentCopyStatus, Store,
};
use dakia_core::{
    mail::mailbox_action_outcome_is_uncertain,
    storage::{MessageRemoteIdentity, OperationJournalEntry},
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
    time::Duration,
};
use url::Url;
use uuid::Uuid;

const REMOTE_SEARCH_CONCURRENCY: usize = 4;
const CLASSIFICATION_BATCH_SIZE: usize = 64;
const CLI_SUBMISSION_CLAIM_TIMEOUT: Duration = Duration::from_secs(125);
const CLI_SUBMISSION_CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLI_SUBMISSION_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);

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
enum AiCommand {
    Summarize(AiMessagesArgs),
}

#[derive(Args)]
struct AiMessagesArgs {
    #[arg(required = true)]
    message_id: Vec<String>,
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
            let message = fetch_message(&store, &args.message_id).await?;
            print_value(&message, cli.json)?;
        }
        Command::Attachment { command } => match command {
            AttachmentCommand::List(args) => {
                let message = fetch_message(&store, &args.message_id).await?;
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
            apply_mailbox_action(&store, &args.message_id, MailboxAction::Archive, cli.json).await?
        }
        Command::Spam(args) => {
            apply_mailbox_action(&store, &args.message_id, MailboxAction::Spam, cli.json).await?
        }
        Command::Trash(args) => {
            require_confirmation(args.yes, "trash")?;
            apply_mailbox_action(
                &store,
                &args.messages.message_id,
                MailboxAction::Trash,
                cli.json,
            )
            .await?
        }
        Command::Delete(args) => {
            require_confirmation(args.yes, "permanently delete")?;
            apply_mailbox_action(
                &store,
                &args.messages.message_id,
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
            let outcome = submit_cli_outgoing(
                &store,
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
            print_send_outcome(&outcome, cli.json)?;
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
    }
    Ok(())
}

/// The desktop and CLI clients deliberately use the same journal payload.
/// `prepared` contains the final SMTP bytes, envelope, and Message-ID, so a
/// later Sent-copy reconciliation never rebuilds a different message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DurableOutgoingPayload {
    draft: ComposeMessage,
    prepared: PreparedOutgoingMessage,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CliSendStatus {
    Queued,
    Accepted,
    SentCopyPending,
    Uncertain,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CliSendUncertainty {
    Delivery,
    SentCopy,
}

struct SentCopyTransition {
    state: &'static str,
    outcome: &'static str,
    error: Option<String>,
    status: CliSendStatus,
    uncertainty: Option<CliSendUncertainty>,
}

fn sent_copy_transition(result: Result<SentCopyOutcome, String>) -> SentCopyTransition {
    match result {
        Ok(SentCopyOutcome::Saved) => SentCopyTransition {
            state: "completed",
            outcome: "smtp_accepted_sent_copy_saved",
            error: None,
            status: CliSendStatus::Accepted,
            uncertainty: None,
        },
        Ok(SentCopyOutcome::Uncertain) => SentCopyTransition {
            state: "uncertain",
            outcome: "smtp_accepted_sent_copy_uncertain",
            error: Some("SMTP accepted; Sent-copy reconciliation is uncertain".into()),
            status: CliSendStatus::Uncertain,
            uncertainty: Some(CliSendUncertainty::SentCopy),
        },
        Err(error) => SentCopyTransition {
            state: "sent_copy_pending",
            outcome: "smtp_accepted_sent_copy_pending",
            error: Some(error),
            status: CliSendStatus::SentCopyPending,
            uncertainty: None,
        },
    }
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct CliSendOutcome {
    operation_id: String,
    status: CliSendStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncertainty: Option<CliSendUncertainty>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persistence_error: Option<String>,
}

enum CliSubmissionClaim {
    Claimed(OperationJournalEntry),
    Finished(CliSendOutcome),
}

fn durable_submission_outcome(operation: &OperationJournalEntry) -> Result<Option<CliSendOutcome>> {
    let accepted = operation.smtp_accepted_at.is_some();
    let outcome = match operation.state.as_str() {
        "queued" | "submitting" => return Ok(None),
        "retry" if accepted => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::SentCopyPending,
            response: None,
            uncertainty: None,
            persistence_error: operation.error.clone(),
        },
        "retry" => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::Queued,
            response: None,
            uncertainty: None,
            persistence_error: operation.error.clone(),
        },
        "sent_copy_pending" => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::SentCopyPending,
            response: None,
            uncertainty: None,
            persistence_error: operation.error.clone(),
        },
        "accepted" | "completed" if accepted => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::Accepted,
            response: None,
            uncertainty: None,
            persistence_error: operation.error.clone(),
        },
        "accepted" | "completed" => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::Uncertain,
            response: None,
            uncertainty: Some(CliSendUncertainty::Delivery),
            persistence_error: Some(
                "durable submission completed without an SMTP acceptance marker".into(),
            ),
        },
        "uncertain" => CliSendOutcome {
            operation_id: operation.operation_id.clone(),
            status: CliSendStatus::Uncertain,
            response: None,
            uncertainty: Some(
                if operation
                    .outcome
                    .as_deref()
                    .is_some_and(|outcome| outcome.contains("sent_copy"))
                {
                    CliSendUncertainty::SentCopy
                } else {
                    CliSendUncertainty::Delivery
                },
            ),
            persistence_error: operation.error.clone(),
        },
        "rejected" | "permanent_failed" => {
            bail!(
                "durable SMTP submission was rejected; do not send again. Operation: {}{}",
                operation.operation_id,
                operation
                    .error
                    .as_deref()
                    .map(|error| format!("; {error}"))
                    .unwrap_or_default()
            );
        }
        state => bail!("outgoing submission journal entry has an unexpected state: {state}"),
    };
    Ok(Some(outcome))
}

async fn wait_for_own_submission_claim(
    store: &Store,
    account_id: Uuid,
    operation_id: &str,
    claim_owner: &str,
) -> Result<CliSubmissionClaim> {
    let deadline = tokio::time::Instant::now() + CLI_SUBMISSION_CLAIM_TIMEOUT;
    let mut next_recovery = tokio::time::Instant::now();
    loop {
        let now = tokio::time::Instant::now();
        if now >= next_recovery {
            // Only claims older than the durable lease are recovered. A live
            // desktop or CLI heartbeat remains `submitting` and is never
            // interrupted by a later CLI invocation.
            store
                .mark_interrupted_operations_uncertain(account_id)
                .await?;
            next_recovery = now + CLI_SUBMISSION_RECOVERY_INTERVAL;
        }
        let operation = store
            .operation_journal_entry(operation_id)
            .await?
            .context("durable SMTP submission disappeared")?;
        if let Some(outcome) = durable_submission_outcome(&operation)? {
            return Ok(CliSubmissionClaim::Finished(outcome));
        }
        if operation.state == "queued" {
            if let Some(claimed) = store
                .claim_operation_by_id(operation_id, claim_owner)
                .await?
            {
                if claimed.kind != "smtp_submission" {
                    bail!("outgoing submission journal entry has an unexpected kind");
                }
                return Ok(CliSubmissionClaim::Claimed(claimed));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(CliSubmissionClaim::Finished(CliSendOutcome {
                operation_id: operation_id.to_owned(),
                status: CliSendStatus::Queued,
                response: None,
                uncertainty: None,
                persistence_error: Some(
                    "another SMTP submission is still active; this message remains queued".into(),
                ),
            }));
        }
        tokio::time::sleep(CLI_SUBMISSION_CLAIM_POLL_INTERVAL).await;
    }
}

/// Sends only an operation that is already durably staged. SMTP acceptance and
/// Sent-copy persistence deliberately have separate journal transitions: a
/// failure to save a copy cannot be presented as a failed delivery.
async fn submit_cli_outgoing(
    store: &Store,
    account: &dakia_core::Account,
    draft: &ComposeMessage,
) -> Result<CliSendOutcome> {
    let service = MailService::new(store.clone());
    let prepared = service.prepare_outgoing_message(account, draft)?;
    let payload = serde_json::to_string(&DurableOutgoingPayload {
        draft: draft.clone(),
        prepared: prepared.clone(),
    })?;
    let submission_owner = Uuid::new_v4().to_string();
    let staged = store
        .enqueue_smtp_submission_and_claim(account.id, &payload, &submission_owner)
        .await?;
    if staged.kind != "smtp_submission" {
        bail!("outgoing submission journal entry has an unexpected kind");
    }
    let staged_operation_id = staged.operation_id.clone();
    let claimed = match staged.state.as_str() {
        "submitting" => staged,
        "queued" => {
            match wait_for_own_submission_claim(
                store,
                account.id,
                &staged_operation_id,
                &submission_owner,
            )
            .await?
            {
                CliSubmissionClaim::Claimed(claimed) => claimed,
                CliSubmissionClaim::Finished(outcome) => return Ok(outcome),
            }
        }
        state => {
            bail!("outgoing submission journal entry has an unexpected state: {state}");
        }
    };
    if claimed.operation_id != staged_operation_id || claimed.state != "submitting" {
        bail!("outgoing submission journal entry has an unexpected state");
    }
    let operation_id = claimed.operation_id.clone();
    let heartbeat = store.heartbeat_operation_claim(operation_id.clone(), submission_owner.clone());
    let smtp_result = service.submit_prepared_smtp(account, &prepared).await;

    match smtp_result {
        Ok(SendOutcome::Accepted {
            response,
            sent_copy: SentCopyStatus::ProviderManaged,
        }) => {
            let persistence_error = store
                .complete_claimed_operation(
                    &operation_id,
                    &submission_owner,
                    "accepted",
                    Some("provider_sent_reconciliation"),
                    None,
                    None,
                )
                .await
                .err()
                .map(|error| error.to_string());
            let outcome = CliSendOutcome {
                operation_id,
                status: CliSendStatus::Accepted,
                response: Some(response),
                uncertainty: None,
                persistence_error,
            };
            drop(heartbeat);
            Ok(outcome)
        }
        Ok(SendOutcome::Accepted {
            response,
            sent_copy: SentCopyStatus::Pending,
        }) => {
            let pending_transition = store
                .complete_claimed_operation(
                    &operation_id,
                    &submission_owner,
                    "sent_copy_pending",
                    Some("smtp_accepted"),
                    None,
                    None,
                )
                .await;
            if let Err(error) = pending_transition {
                // SMTP has accepted the message. The staged artifact remains
                // available for startup recovery, so returning an ordinary
                // error here would invite a duplicate send.
                let outcome = CliSendOutcome {
                    operation_id,
                    status: CliSendStatus::Accepted,
                    response: Some(response),
                    uncertainty: None,
                    persistence_error: Some(error.to_string()),
                };
                drop(heartbeat);
                return Ok(outcome);
            }

            drop(heartbeat);
            Ok(reconcile_cli_sent_copy(
                store,
                account,
                &service,
                &operation_id,
                &prepared,
                response,
            )
            .await)
        }
        Ok(SendOutcome::DeliveryUncertain) => {
            let persistence_error = store
                .complete_claimed_operation(
                    &operation_id,
                    &submission_owner,
                    "uncertain",
                    Some("smtp_delivery_uncertain"),
                    None,
                    None,
                )
                .await
                .err()
                .map(|error| error.to_string());
            let outcome = CliSendOutcome {
                operation_id,
                status: CliSendStatus::Uncertain,
                response: None,
                uncertainty: Some(CliSendUncertainty::Delivery),
                persistence_error,
            };
            drop(heartbeat);
            Ok(outcome)
        }
        Err(failure) => {
            let failure_text = failure.to_string();
            store
                .complete_claimed_operation(
                    &operation_id,
                    &submission_owner,
                    "rejected",
                    Some("rejected_before_acceptance"),
                    Some(&failure_text),
                    None,
                )
                .await
                .context("could not record SMTP rejection")?;
            drop(heartbeat);
            Err(failure)
        }
    }
}

/// A CLI invocation gets one bounded APPEND attempt after the SMTP result is
/// durable. It never retries an ambiguous APPEND, and later desktop work can
/// reconcile the remaining `sent_copy_pending` record.
async fn reconcile_cli_sent_copy(
    store: &Store,
    account: &dakia_core::Account,
    service: &MailService,
    operation_id: &str,
    prepared: &PreparedOutgoingMessage,
    response: String,
) -> CliSendOutcome {
    let owner = Uuid::new_v4().to_string();
    let claimed = match store.claim_sent_copy_operation(operation_id, &owner).await {
        Ok(Some(claimed)) => claimed,
        Ok(None) => {
            return CliSendOutcome {
                operation_id: operation_id.to_owned(),
                status: CliSendStatus::SentCopyPending,
                response: Some(response),
                uncertainty: None,
                persistence_error: None,
            };
        }
        Err(error) => {
            return CliSendOutcome {
                operation_id: operation_id.to_owned(),
                status: CliSendStatus::SentCopyPending,
                response: Some(response),
                uncertainty: None,
                persistence_error: Some(error.to_string()),
            };
        }
    };
    if claimed.kind != "smtp_submission" {
        return CliSendOutcome {
            operation_id: operation_id.to_owned(),
            status: CliSendStatus::SentCopyPending,
            response: Some(response),
            uncertainty: None,
            persistence_error: Some("Sent-copy journal entry has an unexpected kind".into()),
        };
    }

    let heartbeat = store.heartbeat_operation_claim(operation_id.to_owned(), owner.clone());
    let sent_copy_result = service
        .save_pending_sent_copy(account, prepared)
        .await
        .map_err(|error| error.to_string());
    let transition = sent_copy_transition(sent_copy_result);
    let persistence_error = store
        .complete_claimed_operation(
            operation_id,
            &owner,
            transition.state,
            Some(transition.outcome),
            transition.error.as_deref(),
            None,
        )
        .await
        .err()
        .map(|error| error.to_string());
    let outcome = CliSendOutcome {
        operation_id: operation_id.to_owned(),
        status: transition.status,
        response: Some(response),
        uncertainty: transition.uncertainty,
        persistence_error,
    };
    drop(heartbeat);
    outcome
}

fn print_send_outcome(outcome: &CliSendOutcome, json: bool) -> Result<()> {
    if json {
        return print_value(outcome, true);
    }
    match (outcome.status, outcome.uncertainty) {
        (CliSendStatus::Queued, _) => {
            println!(
                "Queued for SMTP delivery. Do not send again. Operation: {}",
                outcome.operation_id
            );
        }
        (CliSendStatus::Accepted, _) => {
            println!(
                "Accepted by SMTP: {}",
                outcome.response.as_deref().unwrap_or_default()
            );
        }
        (CliSendStatus::SentCopyPending, _) => {
            println!(
                "SMTP accepted; Sent copy is pending. Do not send again. Operation: {}",
                outcome.operation_id
            );
        }
        (CliSendStatus::Uncertain, Some(CliSendUncertainty::Delivery)) => {
            println!(
                "SMTP delivery is uncertain. Do not send again. Operation: {}",
                outcome.operation_id
            );
        }
        (CliSendStatus::Uncertain, Some(CliSendUncertainty::SentCopy)) => {
            println!("SMTP accepted; Sent-copy reconciliation is uncertain. Do not send again. Operation: {}", outcome.operation_id);
        }
        (CliSendStatus::Uncertain, None) => {
            println!(
                "SMTP outcome is uncertain. Do not send again. Operation: {}",
                outcome.operation_id
            );
        }
    }
    if let Some(error) = &outcome.persistence_error {
        eprintln!("Local reconciliation state needs attention: {error}. Do not send again.");
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

async fn fetch_message(store: &Store, message_id: &str) -> Result<dakia_core::MailSummary> {
    let identity = store
        .capture_message_remote_identity(message_id)
        .await?
        .context("message not found")?;
    let account_id = Uuid::parse_str(&identity.account_id)
        .context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    fetch_message_for_identity(store, &account, &identity).await
}

async fn fetch_message_for_identity(
    store: &Store,
    account: &dakia_core::Account,
    identity: &MessageRemoteIdentity,
) -> Result<dakia_core::MailSummary> {
    MailService::new(store.clone())
        .fetch_message_for_identity(account, identity, usize::MAX)
        .await
}

async fn apply_mailbox_action(
    store: &Store,
    message_ids: &[String],
    action: MailboxAction,
    json: bool,
) -> Result<()> {
    let mut completed = Vec::with_capacity(message_ids.len());
    for message_id in message_ids {
        let identity = store
            .capture_message_remote_identity(message_id)
            .await?
            .with_context(|| format!("message not found: {message_id}"))?;
        let account_id = Uuid::parse_str(&identity.account_id)
            .with_context(|| format!("invalid account ID for message {message_id}"))?;
        let account = store
            .account(account_id)
            .await?
            .with_context(|| format!("account not found for message {message_id}"))?;
        apply_mailbox_action_for_identity(store, &account, &identity, action)
            .await
            .with_context(|| format!("mailbox action failed for {message_id}"))?;
        completed.push(message_id);
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

/// Stages and claims an immutable provider action before touching IMAP. A
/// confirmed remote result is applied to local membership and terminalized in
/// one SQLite transaction, so a process crash cannot leave an unfenced move.
async fn apply_mailbox_action_for_identity(
    store: &Store,
    account: &dakia_core::Account,
    identity: &MessageRemoteIdentity,
    action: MailboxAction,
) -> Result<()> {
    if identity.account_id != account.id.to_string() {
        bail!("message locator belongs to another account");
    }
    let owner = Uuid::new_v4().to_string();
    let operation = store
        .enqueue_and_apply_mailbox_action_and_claim_for_identity(identity, action, None, &owner)
        .await?;
    if operation.kind != "mailbox_action" {
        bail!("mailbox action journal entry has an unexpected kind");
    }
    let _heartbeat = store.heartbeat_operation_claim(operation.operation_id.clone(), owner.clone());
    let remote_result = MailService::new(store.clone())
        .apply_action_with_expected_uidvalidity(
            account,
            &identity.remote_name,
            u32::try_from(identity.uid).context("stored message has an invalid UID")?,
            action,
            identity.uid_validity,
        )
        .await;
    let destination = match remote_result {
        Ok(destination_uid) => destination_uid,
        Err(error) => {
            let error_text = error.to_string();
            match mailbox_action_failure_state(&error) {
                MailboxActionFailureState::Retry => {
                    store
                        .complete_claimed_operation(
                            &operation.operation_id,
                            &owner,
                            "retry",
                            Some("provider_command_not_started"),
                            Some(&error_text),
                            None,
                        )
                        .await
                        .context("could not record retryable mailbox action failure")?;
                    return Err(error);
                }
                MailboxActionFailureState::Uncertain => {}
            }

            // Only the core's typed post-command outcome carries enough
            // evidence to retain the fence as ambiguous. Connection,
            // authentication, SELECT, and other setup failures stay retryable.
            let persistence_error = store
                .complete_claimed_operation(
                    &operation.operation_id,
                    &owner,
                    "uncertain",
                    Some("remote_outcome_uncertain"),
                    Some(&error_text),
                    None,
                )
                .await
                .err();
            let detail = persistence_error
                .map(|error| format!("; local journal update also failed: {error}"))
                .unwrap_or_default();
            bail!("mailbox action outcome is uncertain; do not repeat this command{detail}");
        }
    };

    let completed = store
        .reconcile_and_complete_claimed_mailbox_action(
            &operation.operation_id,
            &owner,
            mailbox_action_destination(action).unwrap_or_default(),
            destination,
        )
        .await;
    match completed {
        Ok(true) => Ok(()),
        Ok(false) => {
            let error = "provider accepted the mailbox action, but its local receipt is stale";
            let _ = store
                .complete_claimed_operation(
                    &operation.operation_id,
                    &owner,
                    "uncertain",
                    Some("remote_reconciled_local_receipt_stale"),
                    Some(error),
                    None,
                )
                .await;
            bail!("{error}; do not repeat this command");
        }
        Err(error) => {
            let detail = error.to_string();
            let _ = store
                .complete_claimed_operation(
                    &operation.operation_id,
                    &owner,
                    "uncertain",
                    Some("remote_reconciled_local_commit_failed"),
                    Some(&detail),
                    None,
                )
                .await;
            bail!("provider accepted the mailbox action, but local reconciliation needs attention; do not repeat this command");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailboxActionFailureState {
    Retry,
    Uncertain,
}

fn mailbox_action_failure_state(error: &anyhow::Error) -> MailboxActionFailureState {
    if mailbox_action_outcome_is_uncertain(error) {
        MailboxActionFailureState::Uncertain
    } else {
        MailboxActionFailureState::Retry
    }
}

async fn download_attachment(
    store: &Store,
    args: DownloadAttachmentArgs,
    json: bool,
) -> Result<()> {
    let identity = store
        .capture_message_remote_identity(&args.message_id)
        .await?
        .context("message not found")?;
    let account_id = Uuid::parse_str(&identity.account_id)
        .context("stored message has an invalid account ID")?;
    let account = store
        .account(account_id)
        .await?
        .context("account not found")?;
    let attachment = fetch_attachment_for_identity(store, &account, &identity, &args.attachment_id)
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

async fn fetch_attachment_for_identity(
    store: &Store,
    account: &dakia_core::Account,
    identity: &MessageRemoteIdentity,
    attachment_id: &str,
) -> Result<dakia_core::storage::AttachmentData> {
    MailService::new(store.clone())
        .fetch_attachment_for_identity(account, identity, attachment_id)
        .await
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
    use dakia_core::{
        classification::ModelClassification, mail::MoveDestination, provider, AccountDraft,
        MailSummary,
    };
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

    #[test]
    fn accepted_smtp_with_a_failed_sent_copy_stays_pending_not_failed() {
        let transition = sent_copy_transition(Err("IMAP APPEND rejected".into()));

        assert_eq!(transition.state, "sent_copy_pending");
        assert_eq!(transition.outcome, "smtp_accepted_sent_copy_pending");
        assert_eq!(transition.status, CliSendStatus::SentCopyPending);
        assert_eq!(transition.uncertainty, None);
        assert_eq!(transition.error.as_deref(), Some("IMAP APPEND rejected"));
    }

    #[test]
    fn ambiguous_sent_copy_is_fenced_as_uncertain() {
        let transition = sent_copy_transition(Ok(SentCopyOutcome::Uncertain));

        assert_eq!(transition.state, "uncertain");
        assert_eq!(transition.outcome, "smtp_accepted_sent_copy_uncertain");
        assert_eq!(transition.status, CliSendStatus::Uncertain);
        assert_eq!(transition.uncertainty, Some(CliSendUncertainty::SentCopy));
    }

    #[test]
    fn cli_send_json_distinguishes_delivery_uncertainty_from_acceptance() {
        let accepted = CliSendOutcome {
            operation_id: "accepted-operation".into(),
            status: CliSendStatus::Accepted,
            response: Some("250 queued".into()),
            uncertainty: None,
            persistence_error: None,
        };
        let uncertain = CliSendOutcome {
            operation_id: "uncertain-operation".into(),
            status: CliSendStatus::Uncertain,
            response: None,
            uncertainty: Some(CliSendUncertainty::Delivery),
            persistence_error: None,
        };

        assert_eq!(
            serde_json::to_value(accepted).unwrap()["status"],
            "accepted"
        );
        assert_eq!(
            serde_json::to_value(uncertain).unwrap(),
            serde_json::json!({
                "operation_id": "uncertain-operation",
                "status": "uncertain",
                "uncertainty": "delivery"
            })
        );
    }

    fn receipt_test_account() -> dakia_core::Account {
        AccountDraft {
            email: "receipt@example.test".into(),
            display_name: "Receipt test".into(),
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
        .into_account(provider::by_id("gmail").unwrap())
    }

    fn receipt_test_message(account_id: Uuid) -> MailSummary {
        MailSummary {
            id: "old-local-message".into(),
            account_id: account_id.to_string(),
            mailbox: "INBOX".into(),
            uid: 7,
            message_id: Some("<old@example.test>".into()),
            in_reply_to: None,
            reference_ids: None,
            thread_id: "thread-1".into(),
            subject: "Old mailbox generation".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "receipt@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: String::new(),
            body_text: String::new(),
            body_html: None,
            content_state: "headers_only".into(),
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

    fn durable_smtp_operation(
        state: &str,
        outcome: Option<&str>,
        smtp_accepted_at: Option<chrono::DateTime<Utc>>,
    ) -> OperationJournalEntry {
        let now = Utc::now();
        OperationJournalEntry {
            operation_id: "durable-smtp-operation".into(),
            account_id: Uuid::nil().to_string(),
            mailbox: None,
            uid: None,
            uid_validity: None,
            message_id: None,
            kind: "smtp_submission".into(),
            payload_json: r#"{"draft":{},"prepared":{}}"#.into(),
            local_version: 0,
            dependency_id: None,
            state: state.into(),
            outcome: outcome.map(str::to_owned),
            attempts: 1,
            next_retry_at: None,
            error: None,
            smtp_accepted_at,
            claim_owner: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn worker_completed_submission_uses_the_durable_acceptance_marker() {
        let accepted = durable_submission_outcome(&durable_smtp_operation(
            "completed",
            Some("smtp_accepted_sent_copy_saved"),
            Some(Utc::now()),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(accepted.status, CliSendStatus::Accepted);

        let missing_marker = durable_submission_outcome(&durable_smtp_operation(
            "completed",
            Some("smtp_accepted_sent_copy_saved"),
            None,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(missing_marker.status, CliSendStatus::Uncertain);
        assert_eq!(
            missing_marker.uncertainty,
            Some(CliSendUncertainty::Delivery)
        );

        let pending = durable_submission_outcome(&durable_smtp_operation(
            "retry",
            Some("sent_copy_retry_scheduled"),
            Some(Utc::now()),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(pending.status, CliSendStatus::SentCopyPending);

        let uncertain = durable_submission_outcome(&durable_smtp_operation(
            "uncertain",
            Some("smtp_accepted_sent_copy_uncertain"),
            Some(Utc::now()),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(uncertain.status, CliSendStatus::Uncertain);
        assert_eq!(uncertain.uncertainty, Some(CliSendUncertainty::SentCopy));
    }

    #[test]
    fn connection_refused_mailbox_action_is_retryable_before_any_provider_command() {
        let error = anyhow::anyhow!("IMAP connection failed: Connection refused");

        assert_eq!(
            mailbox_action_failure_state(&error),
            MailboxActionFailureState::Retry
        );
    }

    #[tokio::test]
    async fn cli_read_refuses_a_receipt_after_its_uidvalidity_is_replaced() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, true)
            .await
            .unwrap();
        store
            .save_synced_messages(account.id, "INBOX", &[receipt_test_message(account.id)])
            .await
            .unwrap();
        let receipt = store
            .capture_message_remote_identity("old-local-message")
            .await
            .unwrap()
            .unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 8, 1, true)
            .await
            .unwrap();

        let error = fetch_message_for_identity(&store, &account, &receipt)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Message identity changed; reload the mailbox before opening it"
        );
    }

    #[tokio::test]
    async fn cli_mailbox_action_refuses_a_stale_uidvalidity_receipt_before_imap() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, true)
            .await
            .unwrap();
        store
            .save_synced_messages(account.id, "INBOX", &[receipt_test_message(account.id)])
            .await
            .unwrap();
        let receipt = store
            .capture_message_remote_identity("old-local-message")
            .await
            .unwrap()
            .unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 8, 1, true)
            .await
            .unwrap();

        let error = store
            .enqueue_and_apply_mailbox_action_and_claim_for_identity(
                &receipt,
                MailboxAction::Archive,
                None,
                "cli-stale-receipt",
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "message remote identity is stale");
    }

    #[tokio::test]
    async fn cli_mailbox_action_atomically_recovers_local_move_after_remote_success() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 7, 1, true)
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account.id, "Archive", "Archive", 9, 1, true)
            .await
            .unwrap();
        store
            .save_synced_messages(account.id, "INBOX", &[receipt_test_message(account.id)])
            .await
            .unwrap();
        let receipt = store
            .capture_message_remote_identity("old-local-message")
            .await
            .unwrap()
            .unwrap();
        let owner = "cli-local-reconciliation";
        let operation = store
            .enqueue_and_apply_mailbox_action_and_claim_for_identity(
                &receipt,
                MailboxAction::Archive,
                None,
                owner,
            )
            .await
            .unwrap();
        assert_eq!(operation.state, "submitting");
        assert!(store
            .renew_operation_claim(&operation.operation_id, owner)
            .await
            .unwrap());

        assert!(store
            .reconcile_and_complete_claimed_mailbox_action(
                &operation.operation_id,
                owner,
                "Archive",
                Some(MoveDestination {
                    uid_validity: 9,
                    uid: 44,
                }),
            )
            .await
            .unwrap());
        assert!(store
            .message_by_locator(account.id, "INBOX", 7)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .message_by_locator(account.id, "Archive", 44)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .operation_journal_entry(&operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
    }

    #[tokio::test]
    async fn active_cli_smtp_claim_is_not_recovered_while_renewed() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();

        let operation = store
            .enqueue_smtp_submission_and_claim(
                account.id,
                r#"{"draft":{},"prepared":{}}"#,
                "active-cli-send",
            )
            .await
            .unwrap();
        assert_eq!(operation.state, "submitting");
        assert!(store
            .renew_operation_claim(&operation.operation_id, "active-cli-send")
            .await
            .unwrap());
        assert_eq!(
            store
                .mark_interrupted_operations_uncertain(account.id)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .operation_journal_entry(&operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "submitting"
        );
    }

    #[tokio::test]
    async fn queued_cli_smtp_submissions_preserve_payloads_order_and_recover_after_predecessor() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();

        let active = store
            .enqueue_smtp_submission_and_claim(
                account.id,
                r#"{"draft":{"subject":"active"},"prepared":{"rawMessageBase64":"YQ=="}}"#,
                "active-cli-send",
            )
            .await
            .unwrap();
        assert_eq!(active.state, "submitting");
        tokio::time::sleep(Duration::from_millis(2)).await;
        let first_queued = store
            .enqueue_smtp_submission_and_claim(
                account.id,
                r#"{"draft":{"subject":"first queued"},"prepared":{"rawMessageBase64":"Yg=="}}"#,
                "first-queued-cli-send",
            )
            .await
            .unwrap();
        assert_eq!(first_queued.state, "queued");
        tokio::time::sleep(Duration::from_millis(2)).await;
        let second_queued = store
            .enqueue_smtp_submission_and_claim(
                account.id,
                r#"{"draft":{"subject":"second queued"},"prepared":{"rawMessageBase64":"Yw=="}}"#,
                "second-queued-cli-send",
            )
            .await
            .unwrap();
        assert_eq!(second_queued.state, "queued");
        assert_ne!(first_queued.operation_id, second_queued.operation_id);

        let queued: Vec<_> = store
            .pending_operations(account.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|operation| operation.kind == "smtp_submission")
            .collect();
        assert_eq!(
            queued
                .iter()
                .map(|operation| operation.operation_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                first_queued.operation_id.as_str(),
                second_queued.operation_id.as_str()
            ]
        );
        assert_eq!(queued[0].payload_json, first_queued.payload_json);
        assert_eq!(queued[1].payload_json, second_queued.payload_json);

        store
            .complete_claimed_operation(
                &active.operation_id,
                "active-cli-send",
                "uncertain",
                Some("interrupted_before_outcome"),
                Some("fictional expired CLI claim"),
                None,
            )
            .await
            .unwrap();
        let claimed = wait_for_own_submission_claim(
            &store,
            account.id,
            &first_queued.operation_id,
            "first-queued-cli-send",
        )
        .await
        .unwrap();
        match claimed {
            CliSubmissionClaim::Claimed(operation) => {
                assert_eq!(operation.operation_id, first_queued.operation_id);
                assert_eq!(operation.state, "submitting");
            }
            CliSubmissionClaim::Finished(_) => {
                panic!("the queued operation must be claimed by its owner")
            }
        }
    }

    #[tokio::test]
    async fn cli_send_stops_at_the_durable_account_gate_before_smtp() {
        let store = Store::in_memory().await.unwrap();
        let account = receipt_test_account();
        store.save_account(&account).await.unwrap();
        assert!(store
            .begin_account_removal_with_owner(account.id, "cli-send-gate")
            .await
            .unwrap());

        let error = submit_cli_outgoing(
            &store,
            &account,
            &ComposeMessage {
                account_id: account.id,
                to: vec!["recipient@example.test".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "A gated message".into(),
                body_text: "This must never reach SMTP".into(),
                body_html: None,
                in_reply_to: None,
                references: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "account is unavailable for submission");
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
