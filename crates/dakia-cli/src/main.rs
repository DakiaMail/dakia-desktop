use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand};
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, parse_search_query, ComposeMessage, EmailClassificationInput,
    LocalEmailClassifier, MailService, MailboxAction, ModelClassificationUpdate, SearchErrorV2,
    SearchParseError, SearchQuery, Store,
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
async fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(error) = execute(cli).await {
        if json {
            if let Some(error) = error.downcast_ref::<CliSearchError>() {
                // Keep successful `--json search` output as its established
                // array shape. Only an invalid query is an error envelope.
                println!("{}", serde_json::json!({"error": error.error}));
            } else {
                eprintln!("Error: {error:#}");
            }
        } else {
            eprintln!("Error: {error:#}");
        }
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
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
            let remote = args.remote;
            let mut query = compile_cli_search_query(args)?;
            let has_active_scope =
                restrict_cli_search_to_active_accounts(&store, &mut query).await?;
            let results = if !has_active_scope {
                // `SearchQuery.account_ids = []` means "all accounts" to
                // storage, so never pass an empty intersection through. An
                // explicitly disabled or deleted account scope is empty, not
                // an invitation to search another account.
                Vec::new()
            } else if remote {
                search_local_and_remote(&store, &query, cli.json).await?
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

#[derive(Debug)]
struct CliSearchError {
    error: SearchErrorV2,
}

impl std::fmt::Display for CliSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.error.message)
    }
}

impl std::error::Error for CliSearchError {}

fn validate_search_query(raw_query: &str) -> Result<()> {
    parse_search_query(raw_query)
        .map(|_| ())
        .map_err(search_cli_error)
}

/// Converts the CLI's pre-search flags into the shared query language before
/// either local or provider candidate selection begins.  Keeping these flags
/// as `SearchQuery` post-filters used to let a provider spend `--limit` on a
/// newer non-matching result, then discard it after the fact.
///
/// Account IDs remain a separate transport scope: they cannot be represented
/// by the user-facing search language and are applied before either catalogue
/// or provider work begins.
fn compile_cli_search_query(args: SearchArgs) -> Result<SearchQuery> {
    // Validate the user text first so an error location still refers to the
    // command-line query, rather than to our parentheses or appended flags.
    validate_search_query(&args.query)?;

    let SearchArgs {
        query,
        account,
        mailbox,
        from,
        unread,
        limit,
        remote: _,
    } = args;

    let mut legacy_clauses = Vec::new();
    if let Some(mailbox) = mailbox {
        legacy_clauses.push(format!("in:{}", quote_search_literal(&mailbox)));
    }
    if let Some(from) = from {
        legacy_clauses.push(format!("from:{}", quote_search_literal(&from)));
    }
    if unread {
        legacy_clauses.push("is:unread".to_owned());
    }

    // Parenthesizing the original expression preserves `OR` and `NOT`
    // precedence when a legacy narrowing flag is appended as an implicit AND.
    let text = if legacy_clauses.is_empty() {
        query
    } else if query.trim().is_empty() {
        legacy_clauses.join(" ")
    } else {
        format!("({query}) {}", legacy_clauses.join(" "))
    };
    // This validates flag values too, including control characters and the
    // overall shared-language complexity limit.
    validate_search_query(&text)?;

    Ok(SearchQuery {
        text,
        account_ids: account,
        // The legacy criteria now live in `text`. Leaving these unset is
        // important: remote search receives exactly the same expression.
        mailbox: None,
        from: None,
        unread_only: false,
        read_only: false,
        flagged_only: false,
        unflagged_only: false,
        category: None,
        limit: Some(limit),
        cursor: None,
    })
}

fn quote_search_literal(value: &str) -> String {
    // Keep a simple ASCII token unquoted. Besides avoiding unnecessary syntax
    // noise, that lets the shared conservative IMAP compiler use FROM/TO
    // candidate keys for a flag such as `--from alice`. Everything else is
    // quoted so punctuation remains literal rather than becoming query syntax.
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

/// Resolves the CLI's optional account scope to active account IDs for both
/// local and remote search.  Storage deliberately treats an empty ID list as
/// an all-account query, so callers must use the boolean result to distinguish
/// an empty active intersection from the ordinary all-active-accounts case.
async fn restrict_cli_search_to_active_accounts(
    store: &Store,
    query: &mut SearchQuery,
) -> Result<bool> {
    let requested = std::mem::take(&mut query.account_ids);
    let active = store
        .accounts()
        .await?
        .into_iter()
        .filter(|account| {
            account.enabled && (requested.is_empty() || requested.contains(&account.id))
        })
        .map(|account| account.id)
        .collect::<Vec<_>>();
    let has_active_scope = !active.is_empty();
    query.account_ids = active;
    Ok(has_active_scope)
}

fn search_cli_error(error: SearchParseError) -> anyhow::Error {
    let message = error.to_string();
    let mut structured = SearchErrorV2::from(error);
    structured.message = message;
    anyhow::Error::new(CliSearchError { error: structured })
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

struct RemoteSearchOutcome {
    account_id: Uuid,
    account_email: String,
    result: Result<Vec<dakia_core::MailSummary>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteSearchFailure {
    account_id: Uuid,
    account_email: String,
    message: String,
}

async fn search_remote(
    store: &Store,
    query: &SearchQuery,
    json_diagnostics: bool,
) -> Result<Vec<dakia_core::MailSummary>> {
    if query.text.trim().is_empty() {
        bail!("remote search requires a non-empty query");
    }
    // CLI parsing and provider compilation intentionally share the exact
    // same AST. In particular, do not give a provider the raw query text:
    // `from:alice OR subject:invoice` has Dakia semantics, not Gmail's.
    let expression = parse_search_query(&query.text).map_err(search_cli_error)?;
    let accounts = store.accounts().await?;
    let accounts = accounts
        .into_iter()
        .filter(|account| {
            account.enabled
                && (query.account_ids.is_empty() || query.account_ids.contains(&account.id))
        })
        .collect::<Vec<_>>();
    let per_account_limit = query.limit.unwrap_or(100).min(500) as usize;
    let search_store = store.clone();
    let expression = expression.clone();
    let mailbox = query.mailbox.clone();
    let searches = run_bounded_ordered(accounts, REMOTE_SEARCH_CONCURRENCY, move |account| {
        let store = search_store.clone();
        let expression = expression.clone();
        let mailbox = mailbox.clone();
        async move {
            let account_id = account.id;
            let account_email = account.email.clone();
            let result = MailService::new(store)
                .search_remote_expression(
                    &account,
                    &expression,
                    mailbox.as_deref(),
                    per_account_limit,
                )
                .await
                .with_context(|| format!("remote search failed for {account_email}"));
            Ok::<_, anyhow::Error>(RemoteSearchOutcome {
                account_id,
                account_email,
                result,
            })
        }
    })
    .await;
    let outcomes = searches
        .into_iter()
        .collect::<Result<Vec<_>, anyhow::Error>>()?;
    let (results, failures) = aggregate_remote_search_outcomes(outcomes, query, per_account_limit);
    report_remote_search_failures(&failures, json_diagnostics);
    Ok(results)
}

fn aggregate_remote_search_outcomes(
    outcomes: Vec<RemoteSearchOutcome>,
    query: &SearchQuery,
    per_account_limit: usize,
) -> (Vec<dakia_core::MailSummary>, Vec<RemoteSearchFailure>) {
    let mut results = Vec::new();
    let mut failures = Vec::new();
    // `run_bounded_ordered` keeps these outcomes in Store::accounts order.
    // Preserve all successful providers even if a later account is offline.
    for outcome in outcomes {
        match outcome.result {
            Ok(hits) => results.extend(hits.into_iter().filter(|message| {
                (!query.unread_only || !message.is_read)
                    && query
                        .from
                        .as_deref()
                        .map(|from| message.from_address.contains(from))
                        .unwrap_or(true)
            })),
            Err(error) => failures.push(RemoteSearchFailure {
                account_id: outcome.account_id,
                account_email: outcome.account_email,
                message: format!("{error:#}"),
            }),
        }
    }
    results.sort_by_key(|result| std::cmp::Reverse(result.received_at));
    results.truncate(per_account_limit);
    (results, failures)
}

fn report_remote_search_failures(failures: &[RemoteSearchFailure], json_diagnostics: bool) {
    for failure in failures {
        if json_diagnostics {
            // stdout stays the established array shape for scripts. stderr is
            // a JSON Lines coverage diagnostic that callers may opt into.
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "remote_search_coverage",
                    "account_id": failure.account_id,
                    "account_email": failure.account_email,
                    "state": "unavailable",
                    "detail": failure.message,
                })
            );
        } else {
            eprintln!(
                "Warning: remote search unavailable for {}: {}",
                failure.account_email, failure.message
            );
        }
    }
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
    json_diagnostics: bool,
) -> Result<Vec<dakia_core::MailSummary>> {
    let mut results = store.search(query).await?;
    let mut known = results
        .iter()
        .map(|message| message.id.clone())
        .collect::<HashSet<_>>();
    for message in search_remote(store, query, json_diagnostics).await? {
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
    for message_id in message_ids {
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
        let destination_uid = MailService::new(store.clone())
            .apply_action(
                &account,
                &summary.mailbox,
                u32::try_from(summary.uid).context("stored message has an invalid UID")?,
                action,
            )
            .await
            .with_context(|| format!("mailbox action failed for {message_id}"))?;
        store
            .move_message(
                account.id,
                &summary.mailbox,
                u32::try_from(summary.uid).context("stored message has an invalid UID")?,
                mailbox_action_destination(action).unwrap_or_default(),
                destination_uid,
            )
            .await?;
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

    #[test]
    fn remote_search_compiles_legacy_narrowing_before_provider_pagination() {
        let query = compile_cli_search_query(SearchArgs {
            query: "invoice OR receipt".into(),
            account: vec![Uuid::from_u128(7)],
            mailbox: Some("INBOX".into()),
            from: Some("alice".into()),
            unread: true,
            limit: 1,
            remote: true,
        })
        .expect("legacy flags compile into the shared expression");

        // The old implementation kept these fields separate and only applied
        // them after each provider had already returned its one newest hit.
        // They must now be visible to both the canonical evaluator and the
        // generic IMAP candidate compiler before that limit is spent.
        assert_eq!(
            query.text,
            "(invoice OR receipt) in:INBOX from:alice is:unread"
        );
        assert!(query.mailbox.is_none());
        assert!(query.from.is_none());
        assert!(!query.unread_only);

        let expression = parse_search_query(&query.text).expect("combined expression parses");
        let candidate =
            dakia_core::compile_generic_imap(&expression, chrono::Utc::now().date_naive())
                .expect("combined expression compiles for a provider");
        assert!(candidate.criteria.contains("FROM \"alice\""));
        assert!(candidate.criteria.contains("UNSEEN"));
    }

    #[test]
    fn legacy_search_flags_preserve_or_precedence_and_escape_literals() {
        let query = compile_cli_search_query(SearchArgs {
            query: "from:one OR from:two".into(),
            account: Vec::new(),
            mailbox: Some("Projects 2026".into()),
            from: Some("a\\b\"c".into()),
            unread: false,
            limit: 50,
            remote: false,
        })
        .expect("legacy values are safely encoded");

        assert_eq!(
            query.text,
            "(from:one OR from:two) in:\"Projects 2026\" from:\"a\\\\b\\\"c\""
        );
        parse_search_query(&query.text).expect("escaped combined expression parses");
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

    #[test]
    fn remote_search_keeps_successes_when_another_account_fails() {
        let successful_account = Uuid::from_u128(1);
        let failed_account = Uuid::from_u128(2);
        let matching_message = dakia_core::MailSummary {
            id: "successful-message".into(),
            account_id: successful_account.to_string(),
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: "successful-thread".into(),
            subject: "Needle".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: chrono::Utc::now(),
            snippet: "needle".into(),
            body_text: "needle".into(),
            body_html: None,
            content_state: "complete".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: false,
            is_flagged: false,
            is_answered: false,
            is_draft: false,
            has_attachments: false,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: Vec::new(),
        };
        let query = SearchQuery {
            text: "needle".into(),
            limit: Some(50),
            ..Default::default()
        };
        let (results, failures) = aggregate_remote_search_outcomes(
            vec![
                RemoteSearchOutcome {
                    account_id: successful_account,
                    account_email: "success@example.test".into(),
                    result: Ok(vec![matching_message]),
                },
                RemoteSearchOutcome {
                    account_id: failed_account,
                    account_email: "offline@example.test".into(),
                    result: Err(anyhow::anyhow!("connection refused")),
                },
            ],
            &query,
            50,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].account_id, successful_account.to_string());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].account_id, failed_account);
        assert!(failures[0].message.contains("connection refused"));
    }
}
