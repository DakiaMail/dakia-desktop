use anyhow::{bail, Context, Result};
use chrono::Utc;
use dakia_core::mail::{ProbeProgressEvent, ProbeProgressPhase};
use dakia_core::{
    Account, AccountAuth, ComposeMessage, MailService, MailSummary, Security, SendFailure,
    SendFailureKind, Store,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::OnceLock,
    time::Duration,
};
use tempfile::TempDir;
use tokio::time::{timeout, Instant};
use uuid::Uuid;

const PROVIDER_SMOKE_TIMEOUT: Duration = Duration::from_secs(45);
const SMOKE_SUCCESS_MESSAGE: &str =
    "Provider smoke passed: IMAP read-neutral sync and SMTP auth/QUIT completed.";
const SMOKE_FAILURE_MESSAGE: &str =
    "Provider smoke failed; no remote mailbox mutation was requested.";
const ROUNDTRIP_SUCCESS_MESSAGE: &str =
    "Provider roundtrip passed: the dedicated disposable account sent to and received from itself.";
const ROUNDTRIP_FAILURE_MESSAGE: &str =
    "Provider roundtrip failed; a separately confirmed disposable test message may have been submitted.";
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(90);

static EXECUTABLE_SHA256_FOR_EVIDENCE: OnceLock<Option<String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SmokeStage {
    Prepared,
    ImapReadNeutral,
    SmtpAuth,
    SmtpSubmissionNotConfirmed,
    DeliveryUncertain,
    SentCopyFailed,
    InboxSync,
    InboxSearch,
    InboxHydration,
    InboxCache,
    InboxObservation,
    RecoveryStoreOpen,
    RecoveryAccountPersist,
    RecoveryCredentialPersist,
    RecoverySmoke,
    RecoveryHeaderSync,
    RecoveryExactSearch,
    RecoveryBodyFetch,
    /// The second foreground fetch, publication, and canonical reader projection.
    /// Kept separate from the cache lookup so recovery timeouts stay attributable.
    RecoveryHydrate,
    RecoveryCacheVerify,
    /// The persisted foreground-cache lookup and LRU update after hydration.
    RecoveryCacheRead,
    Cleanup,
    Timeout,
    Unknown,
}

const MAX_RECOVERY_DIAGNOSTIC_OPERATIONS: usize = 32;

/// Fixed labels only. Evidence never receives endpoint data, server output,
/// account data, message data, or credential material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecoveryOperation {
    StoreOpen,
    AccountPersist,
    CredentialPersist,
    ImapTcpConnect,
    ImapTlsHandshake,
    ImapStartTls,
    ImapGreeting,
    ImapPostTlsCapability,
    ImapCapability,
    ImapAuthenticate,
    ImapListInbox,
    ImapExamineInbox,
    ImapInboxStatus,
    ImapLogout,
    SmtpConnect,
    SmtpStartTls,
    SmtpEhlo,
    SmtpAuthenticate,
    SmtpQuit,
    HeaderSync,
    ExactSearch,
    FullFetch,
    Hydrate,
    CacheRead,
}

impl From<ProbeProgressPhase> for RecoveryOperation {
    fn from(value: ProbeProgressPhase) -> Self {
        match value {
            ProbeProgressPhase::ImapTcpConnect => Self::ImapTcpConnect,
            ProbeProgressPhase::ImapTlsHandshake => Self::ImapTlsHandshake,
            ProbeProgressPhase::ImapStartTls => Self::ImapStartTls,
            ProbeProgressPhase::ImapGreeting => Self::ImapGreeting,
            ProbeProgressPhase::ImapPostTlsCapability => Self::ImapPostTlsCapability,
            ProbeProgressPhase::ImapCapability => Self::ImapCapability,
            ProbeProgressPhase::ImapAuthenticate => Self::ImapAuthenticate,
            ProbeProgressPhase::ImapListInbox => Self::ImapListInbox,
            ProbeProgressPhase::ImapExamineInbox => Self::ImapExamineInbox,
            ProbeProgressPhase::ImapInboxStatus => Self::ImapInboxStatus,
            ProbeProgressPhase::ImapLogout => Self::ImapLogout,
            ProbeProgressPhase::SmtpConnect => Self::SmtpConnect,
            ProbeProgressPhase::SmtpStartTls => Self::SmtpStartTls,
            ProbeProgressPhase::SmtpEhlo => Self::SmtpEhlo,
            ProbeProgressPhase::SmtpAuthenticate => Self::SmtpAuthenticate,
            ProbeProgressPhase::SmtpQuit => Self::SmtpQuit,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryOperationTiming {
    operation: RecoveryOperation,
    elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    active_operation: Option<RecoveryOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_elapsed_ms: Option<u64>,
    completed_operations: Vec<RecoveryOperationTiming>,
    operations_clamped: bool,
}

#[derive(Debug)]
struct SmokeStageFailure(SmokeStage);

impl std::fmt::Display for SmokeStageFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider smoke stage failed")
    }
}

impl std::error::Error for SmokeStageFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalProbeMode {
    ReadNeutral,
    Roundtrip,
    RoundtripRecovery,
}

impl LocalProbeMode {
    fn remote_mutation_requested(self) -> bool {
        matches!(self, Self::Roundtrip)
    }

    fn inbox_sync_limit(self) -> u8 {
        match self {
            Self::ReadNeutral => 0,
            // The recovery path initializes the local mailbox epoch with a
            // single newest header, then performs an exact remote UUID
            // search. It never relies on that newest-message window to find
            // the target message.
            Self::Roundtrip | Self::RoundtripRecovery => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalExecution {
    ReadNeutral,
    ConfirmedRoundtrip,
    ReadOnlyRecovery,
}

fn select_local_execution(roundtrip: bool, recovery_roundtrip_id: Option<&str>) -> LocalExecution {
    if recovery_roundtrip_id.is_some() {
        LocalExecution::ReadOnlyRecovery
    } else if roundtrip {
        LocalExecution::ConfirmedRoundtrip
    } else {
        LocalExecution::ReadNeutral
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BodyMismatchDiagnostic {
    expected_bytes: usize,
    received_bytes: usize,
    expected_sha256: String,
    received_sha256: String,
}

#[derive(Debug)]
struct RoundtripBodyMismatch(BodyMismatchDiagnostic);

impl std::fmt::Display for RoundtripBodyMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider roundtrip body did not match")
    }
}

impl std::error::Error for RoundtripBodyMismatch {}

#[derive(Debug)]
struct RoundtripInboxNotFound;

impl std::fmt::Display for RoundtripInboxNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider roundtrip message was not found")
    }
}

impl std::error::Error for RoundtripInboxNotFound {}

#[derive(clap::Parser)]
#[command(
    name = "dakia-provider-smoke",
    about = "Run a bounded, credential-safe provider connectivity probe"
)]
struct Args {
    /// A private local JSON configuration with no password field. This requires
    /// --allow-account and reads the password from the controlling terminal.
    #[arg(long, value_name = "PATH", conflicts_with = "write_gmail_template")]
    config_file: Option<PathBuf>,
    /// Exact disposable account address expected in --config-file. Requiring
    /// this separate allowlist prevents a copied configuration from probing a
    /// different mailbox by accident.
    #[arg(long, value_name = "ADDRESS", conflicts_with = "write_gmail_template")]
    allow_account: Option<String>,
    /// Create a new mode-0600, password-free Gmail configuration template.
    /// The path must not already exist.
    #[arg(long, value_name = "PATH")]
    write_gmail_template: Option<PathBuf>,
    /// Address to place in a newly generated Gmail template. The template
    /// writer serializes it as JSON and never accepts a password.
    #[arg(long, value_name = "ADDRESS", requires = "write_gmail_template")]
    template_account: Option<String>,
    /// Write static, non-secret evidence before a local probe. It is updated
    /// with the final safe outcome on success or failure, and is mode 0600.
    #[arg(long, value_name = "PATH", conflicts_with = "write_gmail_template")]
    evidence_report: Option<PathBuf>,
    /// Send one uniquely identified text-only message only to the exact
    /// allowlisted disposable account, then wait for that same account's
    /// INBOX sync to observe it. This is intentionally separate from the
    /// read-neutral default.
    #[arg(
        long,
        conflicts_with = "recover_roundtrip",
        requires_all = [
            "config_file",
            "allow_account",
            "allow_recipient",
            "confirm_disposable_roundtrip",
            "evidence_report"
        ]
    )]
    roundtrip: bool,
    /// Exact recipient allowed for the roundtrip. It must equal
    /// --allow-account, so a local test cannot send to a third party.
    #[arg(long, value_name = "ADDRESS", requires = "roundtrip")]
    allow_recipient: Option<String>,
    /// Required acknowledgement that --roundtrip sends one real test message
    /// to the disposable account itself.
    #[arg(long, requires = "roundtrip")]
    confirm_disposable_roundtrip: bool,
    /// Recheck one previously recorded roundtrip ID without submitting mail.
    /// The ID must come from a prior local evidence report.
    #[arg(
        long,
        value_name = "UUID",
        conflicts_with_all = ["roundtrip", "write_gmail_template"]
    )]
    recover_roundtrip: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialFlow {
    LegacyCi,
    LocalPrompt,
    CiRecovery,
}

fn select_credential_flow(args: &Args) -> Result<CredentialFlow> {
    if let Some(run_id) = args.recover_roundtrip.as_deref() {
        validate_roundtrip_id(run_id)?;
        if args.evidence_report.is_none() {
            bail!("provider roundtrip recovery requires an evidence report");
        }
        return match (&args.config_file, &args.allow_account) {
            (Some(_), Some(_)) => Ok(CredentialFlow::LocalPrompt),
            (None, None) => Ok(CredentialFlow::CiRecovery),
            _ => bail!(
                "provider roundtrip recovery requires both local configuration options or neither"
            ),
        };
    }

    match (&args.config_file, &args.allow_account) {
        (None, None) if args.evidence_report.is_none() => Ok(CredentialFlow::LegacyCi),
        (Some(_), Some(_)) => Ok(CredentialFlow::LocalPrompt),
        (None, None) => {
            bail!("provider smoke evidence requires a local configuration or recovery mode")
        }
        _ => bail!("local provider smoke requires --config-file and --allow-account"),
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSmokeConfig {
    version: u8,
    provider: String,
    #[serde(rename = "accountEmail")]
    account_email: String,
    imap: Endpoint,
    smtp: Endpoint,
    credentials: BTreeMap<String, String>,
}

/// Local configuration deliberately excludes the password. It is separate
/// from the protected CI JSON contract above, which must remain compatible
/// with the existing `PROVIDER_SMOKE_CONFIG` workflow.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalProviderSmokeConfig {
    version: u8,
    provider: String,
    #[serde(rename = "accountEmail")]
    account_email: String,
    imap: Endpoint,
    smtp: Endpoint,
    #[serde(rename = "dedicatedTestMailbox")]
    dedicated_test_mailbox: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GmailTemplate<'a> {
    version: u8,
    provider: &'a str,
    account_email: &'a str,
    imap: TemplateEndpoint<'a>,
    smtp: TemplateEndpoint<'a>,
    dedicated_test_mailbox: &'a str,
}

#[derive(Serialize)]
struct TemplateEndpoint<'a> {
    host: &'a str,
    port: u16,
    security: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalProbeEvidence<'a> {
    schema_version: u8,
    outcome: &'a str,
    stage: SmokeStage,
    stage_history: Vec<SmokeStage>,
    completed_at_utc: String,
    provider: &'a str,
    authentication_method: &'a str,
    account_scope: &'a str,
    operations: Vec<&'a str>,
    read_neutral_imap_message_fetch_limit: u8,
    roundtrip_inbox_sync_limit: u8,
    roundtrip_remote_search_limit: u8,
    roundtrip_message_limit: u8,
    remote_mutation_requested: bool,
    roundtrip_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    roundtrip_observation: Option<RoundtripObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_mismatch: Option<BodyMismatchDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_diagnostics: Option<RecoveryDiagnostics>,
    source_revision: String,
    executable_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoundtripObservation {
    ExactMessageVerified,
    ExactMessageNotFound,
    BodyMismatch,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    host: String,
    port: u16,
    security: EndpointSecurity,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum EndpointSecurity {
    Tls,
    Starttls,
}

impl From<EndpointSecurity> for Security {
    fn from(value: EndpointSecurity) -> Self {
        match value {
            EndpointSecurity::Tls => Self::Tls,
            EndpointSecurity::Starttls => Self::StartTls,
        }
    }
}

impl ProviderSmokeConfig {
    fn validate_fields(&self) -> Result<()> {
        if self.version != 1
            || self.provider.trim().is_empty()
            || self.account_email.trim().is_empty()
            || self.imap.host.trim().is_empty()
            || self.smtp.host.trim().is_empty()
        {
            bail!("invalid provider smoke configuration");
        }
        Ok(())
    }

    fn password(&self) -> Result<&str> {
        self.validate_fields()?;
        let Some((kind, secret)) = self.credentials.iter().next() else {
            bail!("invalid provider smoke credentials");
        };
        if self.credentials.len() != 1
            || !matches!(kind.as_str(), "password" | "appPassword")
            || secret.trim().is_empty()
        {
            // OAuth and other credential kinds are intentionally rejected:
            // this narrow live smoke must not invent a refresh/token flow.
            bail!("unsupported provider smoke credential kind");
        }
        Ok(secret)
    }

    fn account(&self) -> Account {
        Account {
            id: Uuid::new_v4(),
            email: self.account_email.clone(),
            account_name: self.account_email.clone(),
            display_name: "Provider smoke".into(),
            provider_id: self.provider.clone(),
            auth: AccountAuth::Password {
                username: self.account_email.clone(),
            },
            imap_host: self.imap.host.clone(),
            imap_port: self.imap.port,
            imap_security: self.imap.security.into(),
            smtp_host: self.smtp.host.clone(),
            smtp_port: self.smtp.port,
            smtp_security: self.smtp.security.into(),
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        }
    }
}

impl LocalProviderSmokeConfig {
    fn validate_scope(&self, allowed_account: &str) -> Result<()> {
        if allowed_account.trim().is_empty() || allowed_account != self.account_email {
            bail!("local provider smoke account is not allowlisted");
        }
        validate_local_test_mailbox(&self.dedicated_test_mailbox)?;
        let config = ProviderSmokeConfig {
            version: self.version,
            provider: self.provider.clone(),
            account_email: self.account_email.clone(),
            imap: Endpoint {
                host: self.imap.host.clone(),
                port: self.imap.port,
                security: self.imap.security,
            },
            smtp: Endpoint {
                host: self.smtp.host.clone(),
                port: self.smtp.port,
                security: self.smtp.security,
            },
            credentials: BTreeMap::new(),
        };
        config.validate_fields()?;
        validate_gmail_defaults(&config)
    }

    fn into_smoke(self, password: String, allowed_account: &str) -> Result<ProviderSmokeConfig> {
        self.validate_scope(allowed_account)?;
        let config = ProviderSmokeConfig {
            version: self.version,
            provider: self.provider,
            account_email: self.account_email,
            imap: self.imap,
            smtp: self.smtp,
            credentials: BTreeMap::from([(String::from("appPassword"), password)]),
        };
        config.password()?;
        Ok(config)
    }
}

fn validate_local_test_mailbox(mailbox: &str) -> Result<()> {
    if mailbox.trim().is_empty()
        || mailbox.eq_ignore_ascii_case("inbox")
        || mailbox.chars().any(char::is_control)
    {
        bail!("local provider smoke requires a dedicated non-INBOX test mailbox");
    }
    Ok(())
}

fn validate_gmail_defaults(config: &ProviderSmokeConfig) -> Result<()> {
    if !config.provider.eq_ignore_ascii_case("gmail")
        || config.imap.host != "imap.gmail.com"
        || config.imap.port != 993
        || !matches!(config.imap.security, EndpointSecurity::Tls)
        || config.smtp.host != "smtp.gmail.com"
        || config.smtp.port != 465
        || !matches!(config.smtp.security, EndpointSecurity::Tls)
    {
        bail!("local Gmail smoke requires the documented TLS endpoints");
    }
    Ok(())
}

fn read_private_local_config(path: &Path) -> Result<LocalProviderSmokeConfig> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("local provider smoke configuration is unavailable"))?;
    if !metadata.file_type().is_file() {
        bail!("local provider smoke configuration must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("local provider smoke configuration must be private (mode 0600)");
        }
    }
    let raw = fs::read_to_string(path)
        .map_err(|_| anyhow::anyhow!("local provider smoke configuration is unavailable"))?;
    serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("invalid local provider smoke configuration"))
}

fn prompt_local_password() -> Result<String> {
    let password = rpassword::prompt_password("Gmail app password: ")?;
    if password.trim().is_empty() {
        bail!("a Gmail app password is required");
    }
    Ok(password)
}

fn create_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("could not create private provider smoke file"))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn write_gmail_template(path: &Path, account_email: Option<&str>) -> Result<()> {
    let template = GmailTemplate {
        version: 1,
        provider: "gmail",
        account_email: account_email.unwrap_or("replace-with-disposable-address@example.invalid"),
        imap: TemplateEndpoint {
            host: "imap.gmail.com",
            port: 993,
            security: "tls",
        },
        smtp: TemplateEndpoint {
            host: "smtp.gmail.com",
            port: 465,
            security: "tls",
        },
        dedicated_test_mailbox: "Dakia Smoke Test",
    };
    let mut contents = serde_json::to_vec_pretty(&template)?;
    contents.push(b'\n');
    create_private_file(path, &contents)
}

fn source_revision_for_evidence() -> String {
    let Ok(value) = env::var("DAKIA_PROVIDER_SMOKE_SOURCE_REVISION") else {
        return "unavailable".into();
    };
    let value = value.trim();
    if value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit()) {
        value.to_ascii_lowercase()
    } else {
        "unavailable".into()
    }
}

fn executable_sha256_for_evidence() -> Option<String> {
    EXECUTABLE_SHA256_FOR_EVIDENCE
        .get_or_init(|| {
            let executable = env::current_exe().ok()?;
            let bytes = fs::read(executable).ok()?;
            Some(format!("{:x}", Sha256::digest(bytes)))
        })
        .clone()
}

struct LocalEvidenceDetails<'a> {
    mode: LocalProbeMode,
    roundtrip_id: Option<&'a str>,
    roundtrip_observation: Option<RoundtripObservation>,
    body_mismatch: Option<BodyMismatchDiagnostic>,
    recovery_diagnostics: Option<RecoveryDiagnostics>,
}

fn local_evidence_bytes(
    outcome: &'static str,
    stage: SmokeStage,
    stage_history: &[SmokeStage],
    details: &LocalEvidenceDetails<'_>,
) -> Result<Vec<u8>> {
    let mut operations = vec![
        "IMAP read-neutral authentication/discovery",
        "SMTP authentication and QUIT",
    ];
    match details.mode {
        LocalProbeMode::ReadNeutral => {}
        LocalProbeMode::Roundtrip => {
            operations.push("one bounded self roundtrip and exact INBOX UUID observation");
        }
        LocalProbeMode::RoundtripRecovery => {
            operations.push("read-only exact INBOX UUID search and body observation");
        }
    }
    let evidence = LocalProbeEvidence {
        schema_version: 1,
        outcome,
        stage,
        stage_history: stage_history.to_vec(),
        completed_at_utc: Utc::now().to_rfc3339(),
        provider: "gmail",
        authentication_method: "app_password",
        account_scope: "one explicit disposable allowlisted account",
        operations,
        read_neutral_imap_message_fetch_limit: 0,
        roundtrip_inbox_sync_limit: details.mode.inbox_sync_limit(),
        roundtrip_remote_search_limit: match details.mode {
            LocalProbeMode::ReadNeutral => 0,
            LocalProbeMode::Roundtrip | LocalProbeMode::RoundtripRecovery => 10,
        },
        roundtrip_message_limit: u8::from(details.mode.remote_mutation_requested()),
        remote_mutation_requested: details.mode.remote_mutation_requested(),
        roundtrip_id: details.roundtrip_id,
        roundtrip_observation: details.roundtrip_observation,
        body_mismatch: details.body_mismatch.clone(),
        recovery_diagnostics: details.recovery_diagnostics.clone(),
        source_revision: source_revision_for_evidence(),
        executable_sha256: executable_sha256_for_evidence(),
    };
    let mut contents = serde_json::to_vec_pretty(&evidence)?;
    contents.push(b'\n');
    Ok(contents)
}

fn write_local_evidence(
    path: &Path,
    outcome: &'static str,
    stage: SmokeStage,
    mode: LocalProbeMode,
    roundtrip_id: Option<&str>,
) -> Result<()> {
    create_private_file(
        path,
        &local_evidence_bytes(
            outcome,
            stage,
            &[stage],
            &LocalEvidenceDetails {
                mode,
                roundtrip_id,
                roundtrip_observation: None,
                body_mismatch: None,
                recovery_diagnostics: None,
            },
        )?,
    )
}

fn replace_local_evidence(
    path: &Path,
    outcome: &'static str,
    stage: SmokeStage,
    mode: LocalProbeMode,
    roundtrip_id: Option<&str>,
    roundtrip_observation: Option<RoundtripObservation>,
    body_mismatch: Option<BodyMismatchDiagnostic>,
) -> Result<()> {
    let details = LocalEvidenceDetails {
        mode,
        roundtrip_id,
        roundtrip_observation,
        body_mismatch,
        recovery_diagnostics: None,
    };
    replace_local_evidence_with_history(path, outcome, stage, &[stage], &details, true)
}

fn replace_local_evidence_with_history(
    path: &Path,
    outcome: &'static str,
    stage: SmokeStage,
    stage_history: &[SmokeStage],
    details: &LocalEvidenceDetails<'_>,
    durable: bool,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("provider smoke evidence is unavailable"))?;
    if !metadata.file_type().is_file() {
        bail!("provider smoke evidence must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("provider smoke evidence must be private (mode 0600)");
        }
    }
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(&local_evidence_bytes(
        outcome,
        stage,
        stage_history,
        details,
    )?)?;
    if durable {
        file.sync_all()?;
    } else {
        file.flush()?;
    }
    Ok(())
}

trait ProbeStageRecorder {
    fn before(&mut self, stage: SmokeStage) -> Result<()>;
    fn begin_operation(&mut self, _operation: RecoveryOperation) -> Result<()> {
        Ok(())
    }
    fn complete_operation(&mut self, _operation: RecoveryOperation) -> Result<()> {
        Ok(())
    }
    fn probe_progress(&mut self, event: ProbeProgressEvent) -> Result<()> {
        match event {
            ProbeProgressEvent::Started(phase) => self.begin_operation(phase.into()),
            ProbeProgressEvent::Completed(phase) => self.complete_operation(phase.into()),
        }
    }
}

struct NoopProbeStageRecorder;

impl ProbeStageRecorder for NoopProbeStageRecorder {
    fn before(&mut self, _stage: SmokeStage) -> Result<()> {
        Ok(())
    }
}

struct RecoveryEvidenceRecorder<'a> {
    path: &'a Path,
    run_id: &'a str,
    history: Vec<SmokeStage>,
    active_operation: Option<RecoveryOperation>,
    operation_started_at: Option<Instant>,
    completed_operations: Vec<RecoveryOperationTiming>,
    operations_clamped: bool,
}

impl<'a> RecoveryEvidenceRecorder<'a> {
    fn create(path: &'a Path, run_id: &'a str) -> Result<Self> {
        let recorder = Self {
            path,
            run_id,
            history: vec![SmokeStage::Prepared],
            active_operation: None,
            operation_started_at: None,
            completed_operations: Vec::new(),
            operations_clamped: false,
        };
        write_local_evidence(
            path,
            "started",
            SmokeStage::Prepared,
            LocalProbeMode::RoundtripRecovery,
            Some(run_id),
        )?;
        Ok(recorder)
    }

    fn persist(
        &self,
        outcome: &'static str,
        stage: SmokeStage,
        observation: Option<RoundtripObservation>,
        body_mismatch: Option<BodyMismatchDiagnostic>,
        durable: bool,
    ) -> Result<()> {
        let recovery_diagnostics = RecoveryDiagnostics {
            active_operation: self.active_operation,
            active_elapsed_ms: self.operation_started_at.map(elapsed_millis),
            completed_operations: self.completed_operations.clone(),
            operations_clamped: self.operations_clamped,
        };
        let details = LocalEvidenceDetails {
            mode: LocalProbeMode::RoundtripRecovery,
            roundtrip_id: Some(self.run_id),
            roundtrip_observation: observation,
            body_mismatch,
            recovery_diagnostics: Some(recovery_diagnostics),
        };
        replace_local_evidence_with_history(
            self.path,
            outcome,
            stage,
            &self.history,
            &details,
            durable,
        )
    }

    fn finish(
        &mut self,
        outcome: &'static str,
        stage: SmokeStage,
        observation: Option<RoundtripObservation>,
        body_mismatch: Option<BodyMismatchDiagnostic>,
    ) -> Result<()> {
        if self.history.last() != Some(&stage) {
            self.history.push(stage);
        }
        self.persist(outcome, stage, observation, body_mismatch, true)
    }
}

impl ProbeStageRecorder for RecoveryEvidenceRecorder<'_> {
    fn before(&mut self, stage: SmokeStage) -> Result<()> {
        self.history.push(stage);
        self.persist("started", stage, None, None, false)
    }

    fn begin_operation(&mut self, operation: RecoveryOperation) -> Result<()> {
        if self.active_operation.is_some() {
            bail!("provider smoke operation observer became inconsistent");
        }
        self.active_operation = Some(operation);
        self.operation_started_at = Some(Instant::now());
        // This is the last durable state before a possibly slow operation, so
        // an outer timeout identifies the operation that was in flight.
        self.persist(
            "started",
            *self.history.last().unwrap_or(&SmokeStage::Prepared),
            None,
            None,
            true,
        )
    }

    fn complete_operation(&mut self, operation: RecoveryOperation) -> Result<()> {
        if self.active_operation != Some(operation) {
            bail!("provider smoke operation observer completed an unexpected operation");
        }
        if self.completed_operations.len() < MAX_RECOVERY_DIAGNOSTIC_OPERATIONS {
            self.completed_operations.push(RecoveryOperationTiming {
                operation,
                elapsed_ms: elapsed_millis(
                    self.operation_started_at
                        .expect("active provider smoke operation has a start time"),
                ),
            });
        } else {
            self.operations_clamped = true;
        }
        self.active_operation = None;
        self.operation_started_at = None;
        Ok(())
    }
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn measure_recovery_operation<P, T, F>(
    stages: &mut P,
    operation: RecoveryOperation,
    future: F,
) -> Result<T>
where
    P: ProbeStageRecorder,
    F: std::future::Future<Output = Result<T>>,
{
    stages.begin_operation(operation)?;
    let outcome = future.await;
    stages.complete_operation(operation)?;
    outcome
}

async fn run_smoke(config: ProviderSmokeConfig, temp_dir: &TempDir) -> Result<()> {
    let mut stages = NoopProbeStageRecorder;
    run_smoke_with_stages(config, temp_dir, &mut stages).await
}

async fn run_smoke_with_stages<P: ProbeStageRecorder + Send>(
    config: ProviderSmokeConfig,
    temp_dir: &TempDir,
    stages: &mut P,
) -> Result<()> {
    let password = config.password()?.to_owned();
    let account = config.account();
    let store = Store::open(temp_dir.path().join("provider-smoke.db")).await?;
    store.save_account(&account).await?;
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, &password)
        .await?;

    // The public IMAP probe authenticates and performs only CAPABILITY, LIST,
    // read-only EXAMINE INBOX, and constant-size STATUS. It fetches no message
    // content and cannot expand its remote work with mailbox size.
    service
        .imap_auth_probe_with_progress(&account, &mut |event| stages.probe_progress(event))
        .await
        .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::ImapReadNeutral)))?;
    // This authenticates over the configured TLS/STARTTLS path and issues
    // QUIT before MAIL/RCPT/DATA, so no SMTP envelope or body is submitted.
    service
        .smtp_auth_probe_with_progress(&account, &mut |event| stages.probe_progress(event))
        .await
        .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::SmtpAuth)))?;
    Ok(())
}

fn validate_roundtrip_recipient(allowed_account: &str, recipient: Option<&str>) -> Result<()> {
    if recipient != Some(allowed_account) {
        bail!("provider roundtrip recipient is not allowlisted");
    }
    Ok(())
}

async fn run_confirmed_roundtrip(
    config: ProviderSmokeConfig,
    temp_dir: &TempDir,
    recipient: &str,
    run_id: &str,
) -> Result<()> {
    let password = config.password()?.to_owned();
    let account = config.account();
    let store = Store::open(temp_dir.path().join("provider-roundtrip.db")).await?;
    store.save_account(&account).await?;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, &password)
        .await?;

    let subject = format!("Dakia disposable provider roundtrip {run_id}");
    let body = format!("Dakia disposable provider roundtrip {run_id}");
    service
        .send(
            &account,
            &ComposeMessage {
                account_id: account.id,
                to: vec![recipient.to_owned()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: subject.clone(),
                body_text: body.clone(),
                body_html: None,
                in_reply_to: None,
                references: None,
                attachments: Vec::new(),
            },
        )
        .await
        .map_err(|error| anyhow::Error::new(SmokeStageFailure(send_failure_stage(&error))))?;

    // Gmail can take a few seconds to project a self-addressed SMTP delivery
    // into IMAP. Each attempt initializes the local INBOX epoch with one
    // newest header, then uses Dakia's public remote-search path to locate
    // this exact UUID. That avoids assuming the target remains among the
    // newest 25 messages while retaining a bounded, read-neutral lookup.
    let mut stages = NoopProbeStageRecorder;
    for attempt in 0..3 {
        if let Some(message) =
            find_exact_roundtrip_inbox_message(&service, &account, run_id, &mut stages).await?
        {
            return verify_roundtrip_reader_content(
                &service,
                &store,
                &account,
                &message,
                &subject,
                &body,
                &mut stages,
            )
            .await;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    Err(anyhow::Error::new(RoundtripInboxNotFound))
}

/// Locates only the uniquely generated roundtrip message. `search_remote`
/// performs authenticated `UID SEARCH` and read-neutral header retrieval on
/// the provider, so the result does not depend on which messages happened to
/// be selected by the one-header local epoch initialization.
async fn find_exact_roundtrip_inbox_message<P: ProbeStageRecorder>(
    service: &MailService,
    account: &Account,
    run_id: &str,
    stages: &mut P,
) -> Result<Option<MailSummary>> {
    stages.before(SmokeStage::RecoveryHeaderSync)?;
    measure_recovery_operation(
        stages,
        RecoveryOperation::HeaderSync,
        service.sync_inbox(account, 1),
    )
    .await
    .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxSync)))?;
    let subject = roundtrip_subject(run_id);
    stages.before(SmokeStage::RecoveryExactSearch)?;
    Ok(measure_recovery_operation(
        stages,
        RecoveryOperation::ExactSearch,
        service.search_remote(account, run_id, Some("INBOX"), 10),
    )
    .await
    .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxSearch)))?
    .into_iter()
    .find(|message| message.subject == subject))
}

fn roundtrip_subject(run_id: &str) -> String {
    format!("Dakia disposable provider roundtrip {run_id}")
}

/// Reader hydration deliberately returns a lightweight catalogue projection
/// after caching a non-starred message, so its `body_text` is expected to be
/// empty. Verify both the production reader fetch and the cache that the
/// Reader consumes after hydration, rather than treating that lightweight
/// projection as the rendered message body.
async fn verify_roundtrip_reader_content<P: ProbeStageRecorder>(
    service: &MailService,
    store: &Store,
    account: &Account,
    message: &MailSummary,
    subject: &str,
    expected_body: &str,
    stages: &mut P,
) -> Result<()> {
    let uid = u32::try_from(message.uid)
        .context("provider roundtrip recovery has an invalid message UID")?;
    stages.before(SmokeStage::RecoveryBodyFetch)?;
    let fetched = measure_recovery_operation(
        stages,
        RecoveryOperation::FullFetch,
        service.fetch_message(account, "INBOX", uid),
    )
    .await
    .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxHydration)))?;
    if fetched.subject != subject || !body_matches_roundtrip(expected_body, &fetched.body_text) {
        return Err(roundtrip_body_mismatch(expected_body, &fetched.body_text));
    }
    // Preserve the existing broad compatibility marker before refining the
    // individual operations below. Historical evidence therefore remains
    // comparable while new evidence can distinguish a second hydration from
    // the subsequent cache read.
    stages.before(SmokeStage::RecoveryCacheVerify)?;
    stages.before(SmokeStage::RecoveryHydrate)?;
    measure_recovery_operation(
        stages,
        RecoveryOperation::Hydrate,
        service.hydrate_inbox_message(account, uid),
    )
    .await
    .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxHydration)))?;
    stages.before(SmokeStage::RecoveryCacheRead)?;
    let cached = measure_recovery_operation(
        stages,
        RecoveryOperation::CacheRead,
        store.cached_message_content(&message.id),
    )
    .await
    .map_err(|_| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxCache)))?
    .ok_or_else(|| anyhow::Error::new(SmokeStageFailure(SmokeStage::InboxCache)))?;
    if body_matches_roundtrip(expected_body, &cached.body_text) {
        Ok(())
    } else {
        Err(roundtrip_body_mismatch(expected_body, &cached.body_text))
    }
}

fn send_failure_stage(error: &anyhow::Error) -> SmokeStage {
    match error
        .downcast_ref::<SendFailure>()
        .map(|failure| failure.kind)
    {
        Some(SendFailureKind::NotSubmitted) => SmokeStage::SmtpSubmissionNotConfirmed,
        Some(SendFailureKind::DeliveryUncertain) => SmokeStage::DeliveryUncertain,
        Some(SendFailureKind::SentCopyFailed) => SmokeStage::SentCopyFailed,
        None => SmokeStage::SmtpSubmissionNotConfirmed,
    }
}

async fn run_smoke_then_confirmed_roundtrip(
    config: ProviderSmokeConfig,
    temp_dir: &TempDir,
    recipient: &str,
    run_id: &str,
) -> Result<()> {
    // Preserve the evidence ordering: verify read-neutral IMAP and SMTP
    // authentication first. The explicit send only starts after those checks
    // pass, and it remains a separate capability from the default probe.
    run_smoke(config.clone(), temp_dir).await?;
    run_confirmed_roundtrip(config, temp_dir, recipient, run_id).await
}

fn read_legacy_ci_config() -> Result<ProviderSmokeConfig> {
    let raw = env::var("PROVIDER_SMOKE_CONFIG")
        .map_err(|_| anyhow::anyhow!("provider smoke configuration is unavailable"))?;
    serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("invalid provider smoke configuration"))
}

async fn execute_legacy_ci() -> Result<()> {
    let config = read_legacy_ci_config()?;
    let temp_dir = tempfile::Builder::new()
        .prefix("dakia-provider-smoke-")
        .tempdir()?;
    let outcome = timeout(PROVIDER_SMOKE_TIMEOUT, run_smoke(config, &temp_dir)).await;
    // The temporary SQLite database also contains the encrypted credential.
    // Remove it regardless of a timeout, auth failure, or protocol error.
    temp_dir
        .close()
        .context("could not remove temporary provider smoke state")?;
    outcome.map_err(|_| anyhow::anyhow!("provider smoke timed out"))?
}

async fn execute_read_only_recovery(
    config: ProviderSmokeConfig,
    evidence: &Path,
    run_id: &str,
) -> Result<()> {
    let mut stages = RecoveryEvidenceRecorder::create(evidence, run_id)?;
    let temp_dir = tempfile::Builder::new()
        .prefix("dakia-provider-smoke-")
        .tempdir()?;
    let outcome = timeout(
        PROVIDER_SMOKE_TIMEOUT,
        recover_roundtrip(config, &temp_dir, run_id, &mut stages),
    )
    .await;
    if let Err(error) = temp_dir
        .close()
        .context("could not remove temporary provider smoke state")
    {
        stages.finish("failed", SmokeStage::Cleanup, None, None)?;
        return Err(error);
    }
    match outcome {
        Ok(Ok(())) => {
            stages.finish(
                "passed",
                SmokeStage::InboxObservation,
                Some(RoundtripObservation::ExactMessageVerified),
                None,
            )?;
            Ok(())
        }
        Ok(Err(error)) => {
            stages.finish(
                "failed",
                error_stage(&error),
                roundtrip_observation_from_error(&error),
                body_mismatch_from_error(&error),
            )?;
            Err(error)
        }
        Err(_) => {
            stages.finish("failed", SmokeStage::Timeout, None, None)?;
            Err(anyhow::anyhow!("provider smoke timed out"))
        }
    }
}

async fn execute_ci_recovery(evidence: &Path, run_id: &str) -> Result<()> {
    validate_roundtrip_id(run_id)?;
    let config = read_legacy_ci_config()?;
    // CI's protected configuration normally permits generic read-neutral
    // probes. Recovery reads one exact message, so keep this wider capability
    // to Gmail's documented TLS endpoints only.
    validate_ci_recovery_config(&config)?;
    execute_read_only_recovery(config, evidence, run_id).await
}

fn validate_ci_recovery_config(config: &ProviderSmokeConfig) -> Result<()> {
    config.password()?;
    validate_gmail_defaults(config)
}

async fn execute_local(
    config_path: &Path,
    allowed_account: &str,
    evidence: Option<&Path>,
    roundtrip: bool,
    recipient: Option<&str>,
    recovery_roundtrip_id: Option<&str>,
) -> Result<()> {
    let local = read_private_local_config(config_path)?;
    // Do not prompt until the configuration has passed strict file, mailbox,
    // endpoint, and allowlist validation. The password is never placed in an
    // environment variable, command line, report, or long-lived config file.
    local.validate_scope(allowed_account)?;
    if roundtrip {
        validate_roundtrip_recipient(allowed_account, recipient)?;
    }
    if let Some(run_id) = recovery_roundtrip_id {
        validate_roundtrip_id(run_id)?;
    }
    let password = prompt_local_password()?;
    let config = local.into_smoke(password, allowed_account)?;
    let execution = select_local_execution(roundtrip, recovery_roundtrip_id);
    let run_id = match execution {
        LocalExecution::ConfirmedRoundtrip => Some(Uuid::new_v4().simple().to_string()),
        LocalExecution::ReadOnlyRecovery => recovery_roundtrip_id.map(str::to_owned),
        LocalExecution::ReadNeutral => None,
    };
    let mode = match execution {
        LocalExecution::ConfirmedRoundtrip => LocalProbeMode::Roundtrip,
        LocalExecution::ReadOnlyRecovery => LocalProbeMode::RoundtripRecovery,
        LocalExecution::ReadNeutral => LocalProbeMode::ReadNeutral,
    };
    if execution == LocalExecution::ReadOnlyRecovery {
        let run_id = recovery_roundtrip_id.expect("recovery ID selected read-only recovery");
        let evidence = evidence.ok_or_else(|| {
            anyhow::anyhow!("provider roundtrip recovery requires an evidence report")
        })?;
        return execute_read_only_recovery(config, evidence, run_id).await;
    }
    if let Some(path) = evidence {
        write_local_evidence(
            path,
            "started",
            SmokeStage::Prepared,
            mode,
            run_id.as_deref(),
        )?;
    }
    let temp_dir = tempfile::Builder::new()
        .prefix("dakia-provider-smoke-")
        .tempdir()?;
    let outcome = if execution == LocalExecution::ConfirmedRoundtrip {
        timeout(
            ROUNDTRIP_TIMEOUT,
            run_smoke_then_confirmed_roundtrip(
                config,
                &temp_dir,
                recipient.expect("roundtrip recipient validated"),
                run_id.as_deref().expect("roundtrip ID created"),
            ),
        )
        .await
    } else {
        timeout(PROVIDER_SMOKE_TIMEOUT, run_smoke(config, &temp_dir)).await
    };
    temp_dir
        .close()
        .context("could not remove temporary provider smoke state")?;
    match outcome {
        Ok(Ok(())) => {
            if let Some(path) = evidence {
                let stage = if roundtrip || recovery_roundtrip_id.is_some() {
                    SmokeStage::InboxObservation
                } else {
                    SmokeStage::SmtpAuth
                };
                let observation = (roundtrip || recovery_roundtrip_id.is_some())
                    .then_some(RoundtripObservation::ExactMessageVerified);
                replace_local_evidence(
                    path,
                    "passed",
                    stage,
                    mode,
                    run_id.as_deref(),
                    observation,
                    None,
                )?;
            }
            Ok(())
        }
        Ok(Err(error)) => {
            if let Some(path) = evidence {
                replace_local_evidence(
                    path,
                    "failed",
                    error_stage(&error),
                    mode,
                    run_id.as_deref(),
                    roundtrip_observation_from_error(&error),
                    body_mismatch_from_error(&error),
                )?;
            }
            Err(error)
        }
        Err(_) => {
            if let Some(path) = evidence {
                replace_local_evidence(
                    path,
                    "failed",
                    SmokeStage::Timeout,
                    mode,
                    run_id.as_deref(),
                    None,
                    None,
                )?;
            }
            Err(anyhow::anyhow!("provider smoke timed out"))
        }
    }
}

fn error_stage(error: &anyhow::Error) -> SmokeStage {
    if error.downcast_ref::<RoundtripInboxNotFound>().is_some()
        || error.downcast_ref::<RoundtripBodyMismatch>().is_some()
    {
        SmokeStage::InboxObservation
    } else {
        error
            .downcast_ref::<SmokeStageFailure>()
            .map(|failure| failure.0)
            .unwrap_or(SmokeStage::Unknown)
    }
}

fn validate_roundtrip_id(run_id: &str) -> Result<()> {
    if run_id.len() != 32
        || !run_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        bail!("provider roundtrip recovery ID is invalid");
    }
    Ok(())
}

async fn recover_roundtrip<P: ProbeStageRecorder + Send>(
    config: ProviderSmokeConfig,
    temp_dir: &TempDir,
    run_id: &str,
    stages: &mut P,
) -> Result<()> {
    let password = config.password()?.to_owned();
    let account = config.account();
    stages.before(SmokeStage::RecoveryStoreOpen)?;
    let store = measure_recovery_operation(
        stages,
        RecoveryOperation::StoreOpen,
        Store::open(temp_dir.path().join("provider-roundtrip-recovery.db")),
    )
    .await?;
    stages.before(SmokeStage::RecoveryAccountPersist)?;
    measure_recovery_operation(
        stages,
        RecoveryOperation::AccountPersist,
        store.save_account(&account),
    )
    .await?;
    let service = MailService::new(store.clone());
    stages.before(SmokeStage::RecoveryCredentialPersist)?;
    measure_recovery_operation(
        stages,
        RecoveryOperation::CredentialPersist,
        service.credentials().set_password(&account, &password),
    )
    .await?;
    stages.before(SmokeStage::RecoverySmoke)?;
    run_smoke_with_stages(config.clone(), temp_dir, stages).await?;
    let subject = roundtrip_subject(run_id);
    let message = find_exact_roundtrip_inbox_message(&service, &account, run_id, stages)
        .await?
        .ok_or_else(|| anyhow::Error::new(RoundtripInboxNotFound))?;
    verify_roundtrip_reader_content(
        &service, &store, &account, &message, &subject, &subject, stages,
    )
    .await
}

fn roundtrip_body_mismatch(expected: &str, received: &str) -> anyhow::Error {
    anyhow::Error::new(RoundtripBodyMismatch(BodyMismatchDiagnostic {
        expected_bytes: expected.len(),
        received_bytes: received.len(),
        expected_sha256: format!("{:x}", Sha256::digest(expected.as_bytes())),
        received_sha256: format!("{:x}", Sha256::digest(received.as_bytes())),
    }))
}

fn body_mismatch_from_error(error: &anyhow::Error) -> Option<BodyMismatchDiagnostic> {
    error
        .downcast_ref::<RoundtripBodyMismatch>()
        .map(|mismatch| mismatch.0.clone())
}

fn roundtrip_observation_from_error(error: &anyhow::Error) -> Option<RoundtripObservation> {
    if error.downcast_ref::<RoundtripInboxNotFound>().is_some() {
        Some(RoundtripObservation::ExactMessageNotFound)
    } else if error.downcast_ref::<RoundtripBodyMismatch>().is_some() {
        Some(RoundtripObservation::BodyMismatch)
    } else {
        None
    }
}

/// Dakia's production MIME parser removes SMTP's terminal CRLF before
/// presenting text content. Roundtrip validation must compare that decoded
/// value exactly, including any user-authored whitespace.
fn body_matches_roundtrip(expected: &str, received: &str) -> bool {
    received == expected
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = <Args as clap::Parser>::parse();
    if let Some(path) = args.write_gmail_template.as_deref() {
        return match write_gmail_template(path, args.template_account.as_deref()) {
            Ok(()) => {
                println!("Private Gmail provider smoke template created.");
                ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("Provider smoke setup failed; no network connection was opened.");
                ExitCode::FAILURE
            }
        };
    }
    let is_roundtrip = args.roundtrip;
    let outcome = match select_credential_flow(&args) {
        Ok(CredentialFlow::LocalPrompt) => {
            execute_local(
                args.config_file
                    .as_deref()
                    .expect("local config was validated"),
                args.allow_account
                    .as_deref()
                    .expect("local account was validated"),
                args.evidence_report.as_deref(),
                is_roundtrip,
                args.allow_recipient.as_deref(),
                args.recover_roundtrip.as_deref(),
            )
            .await
        }
        Ok(CredentialFlow::LegacyCi) => execute_legacy_ci().await,
        Ok(CredentialFlow::CiRecovery) => {
            execute_ci_recovery(
                args.evidence_report
                    .as_deref()
                    .expect("CI recovery evidence was validated"),
                args.recover_roundtrip
                    .as_deref()
                    .expect("CI recovery ID was validated"),
            )
            .await
        }
        Err(error) => Err(error),
    };
    match outcome {
        Ok(()) => {
            println!(
                "{}",
                if is_roundtrip {
                    ROUNDTRIP_SUCCESS_MESSAGE
                } else {
                    SMOKE_SUCCESS_MESSAGE
                }
            );
            ExitCode::SUCCESS
        }
        Err(_) => {
            // Never include the secret-backed configuration, endpoint, or
            // account identity in CI logs.
            eprintln!(
                "{}",
                if is_roundtrip {
                    ROUNDTRIP_FAILURE_MESSAGE
                } else {
                    SMOKE_FAILURE_MESSAGE
                }
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SECRET: &str = "must-not-appear-in-provider-smoke-output";
    const ROUNDTRIP_ID: &str = "45bacd384f0c4a74893f14e4fea23d9a";

    fn parse_args(values: &[&str]) -> Args {
        <Args as clap::Parser>::try_parse_from(values).expect("parse provider smoke arguments")
    }

    fn gmail_ci_config() -> ProviderSmokeConfig {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "gmail",
            "accountEmail": "disposable@example.test",
            "imap": { "host": "imap.gmail.com", "port": 993, "security": "tls" },
            "smtp": { "host": "smtp.gmail.com", "port": 465, "security": "tls" },
            "credentials": { "appPassword": SECRET },
        }))
        .expect("parse synthetic CI config")
    }

    #[test]
    fn ci_recovery_arguments_select_only_the_read_only_ci_flow() {
        let args = parse_args(&[
            "dakia-provider-smoke",
            "--recover-roundtrip",
            ROUNDTRIP_ID,
            "--evidence-report",
            "private-evidence.json",
        ]);

        assert_eq!(
            select_credential_flow(&args).unwrap(),
            CredentialFlow::CiRecovery
        );
        assert!(!args.roundtrip);
        assert!(!LocalProbeMode::RoundtripRecovery.remote_mutation_requested());
        assert!(<Args as clap::Parser>::try_parse_from([
            "dakia-provider-smoke",
            "--roundtrip",
            "--allow-recipient",
            "disposable@example.test",
            "--confirm-disposable-roundtrip",
            "--evidence-report",
            "private-evidence.json",
        ])
        .is_err());
    }

    #[test]
    fn recovery_argument_selection_rejects_partial_or_mixed_modes_before_any_operation() {
        let missing_evidence =
            parse_args(&["dakia-provider-smoke", "--recover-roundtrip", ROUNDTRIP_ID]);
        assert!(select_credential_flow(&missing_evidence).is_err());

        let partial_local = parse_args(&[
            "dakia-provider-smoke",
            "--recover-roundtrip",
            ROUNDTRIP_ID,
            "--config-file",
            "private-config.json",
            "--evidence-report",
            "private-evidence.json",
        ]);
        assert!(select_credential_flow(&partial_local).is_err());

        let mixed_ci = parse_args(&[
            "dakia-provider-smoke",
            "--recover-roundtrip",
            ROUNDTRIP_ID,
            "--allow-account",
            "disposable@example.test",
            "--evidence-report",
            "private-evidence.json",
        ]);
        assert!(select_credential_flow(&mixed_ci).is_err());

        let stray_evidence = parse_args(&[
            "dakia-provider-smoke",
            "--evidence-report",
            "private-evidence.json",
        ]);
        assert!(select_credential_flow(&stray_evidence).is_err());
    }

    #[test]
    fn local_prompt_recovery_and_no_argument_ci_auth_remain_distinct() {
        let local = parse_args(&[
            "dakia-provider-smoke",
            "--config-file",
            "private-config.json",
            "--allow-account",
            "disposable@example.test",
            "--recover-roundtrip",
            ROUNDTRIP_ID,
            "--evidence-report",
            "private-evidence.json",
        ]);
        assert_eq!(
            select_credential_flow(&local).unwrap(),
            CredentialFlow::LocalPrompt
        );

        let legacy = parse_args(&["dakia-provider-smoke"]);
        assert_eq!(
            select_credential_flow(&legacy).unwrap(),
            CredentialFlow::LegacyCi
        );
    }

    #[test]
    fn local_roundtrip_dispatch_keeps_sending_and_recovery_paths_distinct() {
        assert_eq!(
            select_local_execution(true, None),
            LocalExecution::ConfirmedRoundtrip
        );
        assert_eq!(
            select_local_execution(false, Some(ROUNDTRIP_ID)),
            LocalExecution::ReadOnlyRecovery
        );
        assert_eq!(
            select_local_execution(false, None),
            LocalExecution::ReadNeutral
        );
    }

    #[test]
    fn ci_recovery_requires_gmail_tls_without_echoing_the_secret() {
        validate_ci_recovery_config(&gmail_ci_config())
            .expect("documented Gmail TLS config is accepted");

        for mutate in [
            |config: &mut ProviderSmokeConfig| config.provider = "other".into(),
            |config: &mut ProviderSmokeConfig| config.imap.host = "other.example.test".into(),
            |config: &mut ProviderSmokeConfig| config.imap.port = 143,
            |config: &mut ProviderSmokeConfig| config.imap.security = EndpointSecurity::Starttls,
            |config: &mut ProviderSmokeConfig| config.smtp.host = "other.example.test".into(),
            |config: &mut ProviderSmokeConfig| config.smtp.port = 587,
            |config: &mut ProviderSmokeConfig| config.smtp.security = EndpointSecurity::Starttls,
        ] {
            let mut config = gmail_ci_config();
            mutate(&mut config);
            let error = validate_ci_recovery_config(&config)
                .unwrap_err()
                .to_string();
            assert!(error.contains("documented TLS endpoints"));
            assert!(!error.contains(SECRET));
        }
    }

    #[test]
    fn template_arguments_remain_accepted_without_probe_configuration() {
        assert!(<Args as clap::Parser>::try_parse_from([
            "dakia-provider-smoke",
            "--write-gmail-template",
            "private-template.json",
        ])
        .is_ok());
    }

    #[test]
    fn recovery_stage_evidence_is_ordered_and_non_secret() {
        let temp = tempfile::tempdir().unwrap();
        let evidence_path = temp.path().join("evidence.json");
        let mut recorder = RecoveryEvidenceRecorder::create(&evidence_path, ROUNDTRIP_ID).unwrap();
        for stage in [
            SmokeStage::RecoveryStoreOpen,
            SmokeStage::RecoveryAccountPersist,
            SmokeStage::RecoveryCredentialPersist,
            SmokeStage::RecoverySmoke,
            SmokeStage::RecoveryHeaderSync,
            SmokeStage::RecoveryExactSearch,
            SmokeStage::RecoveryBodyFetch,
            SmokeStage::RecoveryCacheVerify,
            SmokeStage::RecoveryHydrate,
            SmokeStage::RecoveryCacheRead,
        ] {
            recorder.before(stage).unwrap();
        }
        recorder
            .finish("failed", SmokeStage::Timeout, None, None)
            .unwrap();

        let evidence: serde_json::Value =
            serde_json::from_slice(&fs::read(&evidence_path).unwrap()).unwrap();
        assert_eq!(evidence["stage"], "timeout");
        assert_eq!(
            evidence["stageHistory"],
            serde_json::json!([
                "prepared",
                "recovery_store_open",
                "recovery_account_persist",
                "recovery_credential_persist",
                "recovery_smoke",
                "recovery_header_sync",
                "recovery_exact_search",
                "recovery_body_fetch",
                "recovery_cache_verify",
                "recovery_hydrate",
                "recovery_cache_read",
                "timeout",
            ])
        );
        assert_eq!(evidence["remoteMutationRequested"], false);
        assert!(!String::from_utf8(fs::read(evidence_path).unwrap())
            .unwrap()
            .contains(SECRET));
    }

    #[tokio::test]
    async fn recovery_timeout_evidence_retains_the_cancelled_active_operation_without_sensitive_data(
    ) {
        let temp = tempfile::tempdir().unwrap();
        let evidence_path = temp.path().join("evidence.json");
        let mut recorder = RecoveryEvidenceRecorder::create(&evidence_path, ROUNDTRIP_ID).unwrap();
        recorder.before(SmokeStage::RecoveryHeaderSync).unwrap();
        recorder
            .begin_operation(RecoveryOperation::HeaderSync)
            .unwrap();
        recorder
            .complete_operation(RecoveryOperation::HeaderSync)
            .unwrap();
        recorder.before(SmokeStage::RecoveryExactSearch).unwrap();
        let timeout_result = timeout(
            Duration::from_millis(1),
            measure_recovery_operation(
                &mut recorder,
                RecoveryOperation::ExactSearch,
                std::future::pending::<Result<()>>(),
            ),
        )
        .await;
        assert!(
            timeout_result.is_err(),
            "fixture operation must be cancelled by timeout"
        );
        recorder
            .finish("failed", SmokeStage::Timeout, None, None)
            .unwrap();

        let bytes = fs::read(&evidence_path).unwrap();
        let evidence: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let diagnostics = &evidence["recoveryDiagnostics"];
        assert_eq!(diagnostics["activeOperation"], "exact_search");
        assert!(diagnostics["activeElapsedMs"].is_u64());
        let completed = diagnostics["completedOperations"].as_array().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["operation"], "header_sync");
        assert!(completed[0]["elapsedMs"].is_u64());
        assert_eq!(diagnostics["operationsClamped"], false);
        let contents = String::from_utf8(bytes).unwrap();
        for forbidden in [SECRET, "example.test", "AUTH", "message body"] {
            assert!(
                !contents.contains(forbidden),
                "evidence exposed {forbidden:?}"
            );
        }
    }

    #[test]
    fn recovery_operation_evidence_clamps_to_a_fixed_bound() {
        let temp = tempfile::tempdir().unwrap();
        let evidence_path = temp.path().join("evidence.json");
        let mut recorder = RecoveryEvidenceRecorder::create(&evidence_path, ROUNDTRIP_ID).unwrap();
        recorder.before(SmokeStage::RecoveryHeaderSync).unwrap();
        for _ in 0..=MAX_RECOVERY_DIAGNOSTIC_OPERATIONS {
            recorder
                .begin_operation(RecoveryOperation::HeaderSync)
                .unwrap();
            recorder
                .complete_operation(RecoveryOperation::HeaderSync)
                .unwrap();
        }
        recorder
            .finish("failed", SmokeStage::Timeout, None, None)
            .unwrap();

        let evidence: serde_json::Value =
            serde_json::from_slice(&fs::read(&evidence_path).unwrap()).unwrap();
        let diagnostics = &evidence["recoveryDiagnostics"];
        assert_eq!(
            diagnostics["completedOperations"].as_array().unwrap().len(),
            MAX_RECOVERY_DIAGNOSTIC_OPERATIONS
        );
        assert_eq!(diagnostics["operationsClamped"], true);
        assert!(diagnostics.get("activeOperation").is_none());
    }

    #[test]
    fn recovery_evidence_keeps_the_historical_cache_marker_and_attributes_a_stalled_hydration() {
        let temp = tempfile::tempdir().unwrap();
        let evidence_path = temp.path().join("evidence.json");
        let mut recorder = RecoveryEvidenceRecorder::create(&evidence_path, ROUNDTRIP_ID).unwrap();
        for stage in [
            SmokeStage::RecoveryBodyFetch,
            SmokeStage::RecoveryCacheVerify,
            SmokeStage::RecoveryHydrate,
        ] {
            recorder.before(stage).unwrap();
        }
        recorder
            .finish("failed", SmokeStage::Timeout, None, None)
            .unwrap();

        let evidence: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence_path).unwrap()).unwrap();
        assert_eq!(
            evidence["stageHistory"],
            serde_json::json!([
                "prepared",
                "recovery_body_fetch",
                "recovery_cache_verify",
                "recovery_hydrate",
                "timeout",
            ])
        );
        assert_eq!(evidence["remoteMutationRequested"], false);
    }

    #[tokio::test]
    async fn recovery_pipeline_persists_actual_stages_before_its_first_remote_action() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind a local-only IMAP failure fixture");
        let port = listener
            .local_addr()
            .expect("read local-only IMAP fixture address")
            .port();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let accepted = std::thread::spawn(move || {
            let (_connection, _) = listener
                .accept()
                .expect("accept the fixture IMAP connection");
            let _ = accepted_tx.send(());
        });
        let config: ProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "synthetic",
            "accountEmail": "synthetic@example.test",
            "imap": { "host": "127.0.0.1", "port": port, "security": "tls" },
            "smtp": { "host": "127.0.0.1", "port": port, "security": "tls" },
            "credentials": { "appPassword": SECRET },
        }))
        .expect("parse synthetic recovery config");
        let temp = tempfile::tempdir().unwrap();
        let evidence_path = temp.path().join("evidence.json");

        let error = execute_read_only_recovery(config, &evidence_path, ROUNDTRIP_ID)
            .await
            .unwrap_err()
            .to_string();
        let production_connection = accepted_rx.recv_timeout(Duration::from_secs(1));
        if production_connection.is_err() {
            // If a regression fails before the production IMAP connection,
            // wake the fixture so its thread cannot hide that failure by
            // blocking here.
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        }
        accepted
            .join()
            .expect("local-only IMAP failure fixture completed");
        assert!(
            production_connection.is_ok(),
            "the recovery flow must reach the local IMAP fixture"
        );
        assert_eq!(error, "provider smoke stage failed");

        let contents = fs::read_to_string(&evidence_path).unwrap();
        let evidence: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(evidence["stage"], "imap_read_neutral");
        assert_eq!(
            evidence["stageHistory"],
            serde_json::json!([
                "prepared",
                "recovery_store_open",
                "recovery_account_persist",
                "recovery_credential_persist",
                "recovery_smoke",
                "imap_read_neutral",
            ])
        );
        assert_eq!(evidence["remoteMutationRequested"], false);
        assert!(!contents.contains(SECRET));
    }

    #[test]
    fn live_smoke_rejects_oauth_style_credentials_without_echoing_their_value() {
        let config: ProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "test-provider",
            "accountEmail": "smoke@example.invalid",
            "imap": { "host": "imap.example.invalid", "port": 993, "security": "tls" },
            "smtp": { "host": "smtp.example.invalid", "port": 465, "security": "tls" },
            "credentials": { "accessToken": SECRET },
        }))
        .unwrap();

        let error = config.password().unwrap_err().to_string();
        assert!(error.contains("unsupported provider smoke credential kind"));
        assert!(!error.contains(SECRET));
    }

    #[test]
    fn binary_log_messages_are_static_and_do_not_echo_config_values() {
        for message in [SMOKE_SUCCESS_MESSAGE, SMOKE_FAILURE_MESSAGE] {
            assert!(!message.contains(SECRET));
            assert!(!message.contains("example.invalid"));
            assert!(!message.contains("host"));
        }
    }

    #[test]
    fn local_gmail_configuration_requires_an_exact_account_allowlist_and_dedicated_scope() {
        let local: LocalProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "gmail",
            "accountEmail": "disposable@example.test",
            "imap": { "host": "imap.gmail.com", "port": 993, "security": "tls" },
            "smtp": { "host": "smtp.gmail.com", "port": 465, "security": "tls" },
            "dedicatedTestMailbox": "Dakia Smoke Test"
        }))
        .unwrap();
        assert!(local
            .into_smoke("secret".into(), "different@example.test")
            .is_err());

        let inbox: LocalProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "gmail",
            "accountEmail": "disposable@example.test",
            "imap": { "host": "imap.gmail.com", "port": 993, "security": "tls" },
            "smtp": { "host": "smtp.gmail.com", "port": 465, "security": "tls" },
            "dedicatedTestMailbox": "INBOX"
        }))
        .unwrap();
        assert!(inbox
            .into_smoke("secret".into(), "disposable@example.test")
            .is_err());
    }

    #[test]
    fn local_gmail_configuration_rejects_endpoint_downgrades() {
        let local: LocalProviderSmokeConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "provider": "gmail",
            "accountEmail": "disposable@example.test",
            "imap": { "host": "imap.gmail.com", "port": 143, "security": "starttls" },
            "smtp": { "host": "smtp.gmail.com", "port": 465, "security": "tls" },
            "dedicatedTestMailbox": "Dakia Smoke Test"
        }))
        .unwrap();
        assert!(local
            .into_smoke("secret".into(), "disposable@example.test")
            .is_err());
    }

    #[test]
    fn gmail_template_and_evidence_are_password_free() {
        let temp = tempfile::tempdir().unwrap();
        let template = temp.path().join("template.json");
        write_gmail_template(&template, None).unwrap();
        let template_contents = fs::read_to_string(&template).unwrap();
        assert!(template_contents.contains("imap.gmail.com"));
        assert!(!template_contents.contains("password"));

        let evidence = temp.path().join("evidence.json");
        write_local_evidence(
            &evidence,
            "passed",
            SmokeStage::SmtpAuth,
            LocalProbeMode::ReadNeutral,
            None,
        )
        .unwrap();
        let evidence_contents = fs::read_to_string(&evidence).unwrap();
        assert!(evidence_contents.contains("remoteMutationRequested"));
        assert!(!evidence_contents.contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(template).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(evidence).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failure_evidence_keeps_the_roundtrip_id_and_safe_stage_without_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = temp.path().join("evidence.json");
        write_local_evidence(
            &evidence,
            "started",
            SmokeStage::Prepared,
            LocalProbeMode::RoundtripRecovery,
            Some("45bacd384f0c4a74893f14e4fea23d9a"),
        )
        .unwrap();
        replace_local_evidence(
            &evidence,
            "failed",
            SmokeStage::InboxObservation,
            LocalProbeMode::RoundtripRecovery,
            Some("45bacd384f0c4a74893f14e4fea23d9a"),
            Some(RoundtripObservation::ExactMessageNotFound),
            None,
        )
        .unwrap();
        let contents = fs::read_to_string(evidence).unwrap();
        assert!(contents.contains("\"outcome\": \"failed\""));
        assert!(contents.contains("\"stage\": \"inbox_observation\""));
        assert!(contents.contains("45bacd384f0c4a74893f14e4fea23d9a"));
        assert!(!contents.contains(SECRET));
    }

    #[test]
    fn recovery_evidence_records_no_body_text_but_distinguishes_an_exact_body_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = temp.path().join("evidence.json");
        let error = roundtrip_body_mismatch("expected fixture", "received fixture");
        write_local_evidence(
            &evidence,
            "started",
            SmokeStage::Prepared,
            LocalProbeMode::RoundtripRecovery,
            Some("45bacd384f0c4a74893f14e4fea23d9a"),
        )
        .unwrap();
        replace_local_evidence(
            &evidence,
            "failed",
            error_stage(&error),
            LocalProbeMode::RoundtripRecovery,
            Some("45bacd384f0c4a74893f14e4fea23d9a"),
            roundtrip_observation_from_error(&error),
            body_mismatch_from_error(&error),
        )
        .unwrap();

        let contents = fs::read_to_string(evidence).unwrap();
        assert!(contents.contains("\"roundtripObservation\": \"body_mismatch\""));
        assert!(contents.contains("\"bodyMismatch\""));
        assert!(contents.contains("\"expectedBytes\": 16"));
        assert!(!contents.contains("expected fixture"));
        assert!(!contents.contains("received fixture"));
    }

    #[test]
    fn recovery_mode_describes_its_read_only_exact_search_in_evidence() {
        let contents = local_evidence_bytes(
            "started",
            SmokeStage::Prepared,
            &[SmokeStage::Prepared],
            &LocalEvidenceDetails {
                mode: LocalProbeMode::RoundtripRecovery,
                roundtrip_id: Some("45bacd384f0c4a74893f14e4fea23d9a"),
                roundtrip_observation: None,
                body_mismatch: None,
                recovery_diagnostics: None,
            },
        )
        .unwrap();
        let contents = String::from_utf8(contents).unwrap();
        assert!(contents.contains("read-only exact INBOX UUID search"));
        assert!(contents.contains("\"roundtripInboxSyncLimit\": 1"));
        assert!(contents.contains("\"roundtripRemoteSearchLimit\": 10"));
        assert!(contents.contains("\"remoteMutationRequested\": false"));
    }

    #[test]
    fn local_configuration_refuses_group_or_world_readable_files() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        fs::write(&config, "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_private_local_config(&config).is_err());
        }
    }

    #[test]
    fn roundtrip_recipient_must_be_the_exact_allowlisted_disposable_account() {
        assert!(validate_roundtrip_recipient("disposable@example.test", None).is_err());
        assert!(
            validate_roundtrip_recipient("disposable@example.test", Some("else@example.test"))
                .is_err()
        );
        assert!(validate_roundtrip_recipient(
            "disposable@example.test",
            Some("disposable@example.test")
        )
        .is_ok());
    }

    #[test]
    fn roundtrip_body_requires_exact_production_decoded_text() {
        assert!(body_matches_roundtrip("fixture", "fixture"));
        assert!(!body_matches_roundtrip("fixture", "fixture\r\n"));
        assert!(!body_matches_roundtrip("fixture", "fixture\n"));
        assert!(!body_matches_roundtrip("fixture", "fixture\r\n\r\n"));
        assert!(!body_matches_roundtrip("fixture ", "fixture\r\n"));
    }
}
