mod account_locks;
mod realtime;
mod send_recovery_bridge;
mod startup_error;
mod translation;

#[cfg(test)]
mod ai_action_boundary_tests;
#[cfg(all(test, feature = "native-mail-safety-fixture"))]
mod attachment_command_artifact_tests;
#[cfg(feature = "native-mail-safety-fixture")]
mod attachment_partial_fault;
#[cfg(test)]
mod attachment_pipeline_tests;
#[cfg(test)]
mod bridge_security_tests;
#[cfg(test)]
mod l011_sync_interaction_tests;
#[cfg(test)]
mod local_search_command_tests;
#[cfg(test)]
mod mail_action_identity_tests;
#[cfg(test)]
mod mail_command_identity_tests;
#[cfg(all(test, feature = "native-mail-safety-fixture"))]
mod mail_command_tls_tests;
#[cfg(test)]
mod mail_safety_tests;
#[cfg(test)]
mod mime_reader_artifact_tests;
#[cfg(feature = "native-mail-safety-fixture")]
mod native_acl_acceptance;
#[cfg(test)]
mod tauri_contracts_tests;
#[cfg(test)]
mod test_support;
#[cfg(all(test, feature = "native-mail-safety-fixture"))]
mod unsubscribe_command_tls_tests;
#[cfg(all(test, target_os = "macos"))]
mod updater_acceptance_tests;

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use dakia_core::storage::{ConversationTarget, MessageContentFetchAcquire};
#[cfg(test)]
use dakia_core::CachedMessageContent;
#[cfg(all(test, feature = "native-mail-safety-fixture"))]
use dakia_core::NativeMailSafetyOneClickTransport;
use dakia_core::{
    ai::{AiConfig, AiProvider, AiService},
    mailbox_action_destination, normalize_sender_address, provider, Account, AccountAuth,
    AccountDraft, Attachment, ComposeMessage, EmailClassificationInput, LocalEmailClassifier,
    MailConversation, MailConversationPage, MailRebuildJob, MailService, MailSummary,
    MailboxAction, ModelClassificationUpdate, ProviderPreset, SearchQuery, SenderTrashResult,
    SmartInboxPage, SmartInboxQuery, Store, SyncProgress, SyncResult, UnsubscribeOutcome,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    future::Future,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(unix)]
use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd},
};
#[cfg(target_os = "macos")]
use std::{
    ffi::{c_void, OsString},
    os::unix::ffi::OsStringExt,
    ptr,
};
use tauri::{
    image::Image,
    ipc::Channel,
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    DragDropEvent, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{watch, Notify, Semaphore};
use url::Url;
use uuid::Uuid;

use account_locks::AccountOperationLocks;
use realtime::{RealtimeSyncManager, RealtimeSyncStatus};
use send_recovery_bridge::{
    acknowledge_send_recovery_seed, list_send_recoveries, open_send_recovery_window,
    read_send_recovery_seed, SendRecoveryRuntime,
};
use startup_error::{
    present_startup_storage_failure, startup_storage_failure_exit_code,
    TauriStartupFailurePresenter,
};
use translation::{
    TranslationDownloadProgress, TranslationLanguageDetection, TranslationModelFiles,
    TranslationModelStatus,
};

fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    let context = tauri::generate_context!();
    #[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
    {
        let mut context = context;
        apply_native_mail_safety_data_store_identifier(&mut context)
            .expect("native mail-safety data-store configuration is invalid");
        context
    }
    #[cfg(not(all(feature = "native-mail-safety-fixture", target_os = "macos")))]
    {
        context
    }
}

const REMOTE_SEARCH_CONCURRENCY: usize = 4;
const MESSAGE_HYDRATION_CONCURRENCY: usize = 4;
const CLASSIFICATION_BATCH_SIZE: usize = 64;
// AI UI is deliberately hidden while Dakia has no trusted native configuration
// boundary for provider endpoints or local executables. Keep native commands
// disabled as well: a forged IPC request must not read a stored key, send mail
// content, or start a process.
const AI_FEATURES_ENABLED: bool = false;
const AI_FEATURES_DISABLED_ERROR: &str = "AI features are disabled in this Dakia build";
const APP_MENU_QUIT_ID: &str = "app-quit";

#[derive(Clone)]
struct StartupFailureExitStatus {
    exit_requested: Arc<AtomicBool>,
    fatal_recovery_active: Arc<AtomicBool>,
}

impl Default for StartupFailureExitStatus {
    fn default() -> Self {
        Self {
            exit_requested: Arc::new(AtomicBool::new(false)),
            fatal_recovery_active: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl StartupFailureExitStatus {
    fn activate_fatal_recovery(&self) {
        self.fatal_recovery_active.store(true, Ordering::SeqCst);
    }

    fn may_exit_from_fatal_recovery_window(&self, label: &str) -> bool {
        self.fatal_recovery_active.load(Ordering::SeqCst)
            && label == startup_error::FATAL_RECOVERY_WINDOW_LABEL
    }

    fn mark_requested(&self) {
        self.exit_requested.store(true, Ordering::SeqCst);
    }

    fn app_menu_quit_exit_code(&self, action: &str) -> Option<i32> {
        (action == APP_MENU_QUIT_ID).then(|| {
            if self.fatal_recovery_active.load(Ordering::SeqCst) {
                self.mark_requested();
                startup_storage_failure_exit_code()
            } else {
                0
            }
        })
    }

    fn resolve(&self, runtime_exit_code: i32) -> i32 {
        if self.fatal_recovery_active.load(Ordering::SeqCst)
            || self.exit_requested.load(Ordering::SeqCst)
        {
            startup_storage_failure_exit_code()
        } else {
            runtime_exit_code
        }
    }
}

#[cfg(test)]
mod startup_failure_exit_status_tests {
    use super::StartupFailureExitStatus;

    #[test]
    fn startup_failure_survives_a_runtime_zero_exit() {
        let exit_status = StartupFailureExitStatus::default();

        exit_status.mark_requested();

        assert_eq!(exit_status.resolve(0), 1);
    }

    #[test]
    fn fatal_recovery_survives_an_unrequested_runtime_zero_exit() {
        let exit_status = StartupFailureExitStatus::default();

        exit_status.activate_fatal_recovery();

        // A stock menu or Cmd-Q exit reaches run_return without the recovery
        // page callback. It must still preserve the fail-closed startup status.
        assert_eq!(exit_status.resolve(0), 1);
    }

    #[test]
    fn runtime_status_is_preserved_without_a_startup_failure() {
        let exit_status = StartupFailureExitStatus::default();

        assert_eq!(exit_status.resolve(73), 73);
    }

    #[test]
    fn startup_failure_status_is_shared_with_the_fatal_window_callback() {
        let exit_status = StartupFailureExitStatus::default();
        let callback_status = exit_status.clone();

        callback_status.mark_requested();

        assert_eq!(exit_status.resolve(0), 1);
    }

    #[test]
    fn fatal_recovery_exit_requires_both_active_startup_failure_and_exact_window_label() {
        let exit_status = StartupFailureExitStatus::default();

        assert!(!exit_status.may_exit_from_fatal_recovery_window("fatal-recovery"));
        exit_status.activate_fatal_recovery();
        assert!(exit_status.may_exit_from_fatal_recovery_window("fatal-recovery"));
        assert!(!exit_status.may_exit_from_fatal_recovery_window("main"));
        assert!(!exit_status.may_exit_from_fatal_recovery_window("fatal-recovery-copy"));
    }

    #[test]
    fn explicit_app_quit_menu_routes_fatal_recovery_to_a_nonzero_runtime_exit() {
        let normal = StartupFailureExitStatus::default();
        assert_eq!(normal.app_menu_quit_exit_code("unrelated-action"), None);
        assert_eq!(
            normal.app_menu_quit_exit_code(super::APP_MENU_QUIT_ID),
            Some(0)
        );
        assert_eq!(normal.resolve(0), 0);

        let fatal = StartupFailureExitStatus::default();
        fatal.activate_fatal_recovery();
        assert_eq!(
            fatal.app_menu_quit_exit_code(super::APP_MENU_QUIT_ID),
            Some(1)
        );
        assert_eq!(fatal.resolve(0), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_menu_quit_spec_uses_the_regular_cmd_q_event_item() {
        assert_eq!(
            super::macos_app_menu_quit_spec(),
            super::MacosAppMenuQuitSpec {
                id: super::APP_MENU_QUIT_ID,
                text: "Quit Dakia",
                accelerator: "CmdOrCtrl+Q",
            }
        );
    }
}

fn require_ai_features_enabled() -> Result<(), String> {
    AI_FEATURES_ENABLED
        .then_some(())
        .ok_or_else(|| AI_FEATURES_DISABLED_ERROR.to_owned())
}
const CLASSIFICATION_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(500)];
const MAX_EXPORT_FILENAME_BYTES: usize = 255;
const MAX_DOWNLOAD_COLLISION_SUFFIX_BYTES: usize = " (9999)".len();
#[cfg(feature = "native-mail-safety-fixture")]
const NATIVE_MAIL_SAFETY_FIXTURE_MARKER: &str = "dakia-native-mail-safety-fixture-v1\n";
#[cfg(feature = "native-mail-safety-fixture")]
const NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_ID: &str = "62a9320c-04f5-47c2-b4fa-d4952f0b6121";
#[cfg(feature = "native-mail-safety-fixture")]
const NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_EMAIL: &str = "fictional-reader@localhost";
#[cfg(feature = "native-mail-safety-fixture")]
const NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_ID: &str = "62a9320c-04f5-47c2-b4fa-d4952f0b6122";
#[cfg(feature = "native-mail-safety-fixture")]
const NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_EMAIL: &str = "fictional-reader-two@localhost";

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_second_account_enabled(
    value: Result<String, std::env::VarError>,
) -> anyhow::Result<bool> {
    match value {
        Ok(value) if value == "1" => Ok(true),
        Ok(_) => anyhow::bail!("native mail-safety second-account marker must equal 1"),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_profile_directory(
    fixture_root: &Path,
    data_dir: PathBuf,
) -> anyhow::Result<PathBuf> {
    if !data_dir.is_absolute() {
        anyhow::bail!("native mail-safety fixture data directory must be absolute");
    }
    let data_dir = data_dir.canonicalize().map_err(|error| {
        anyhow::anyhow!("canonicalize native mail-safety fixture data directory: {error}")
    })?;
    if data_dir == fixture_root || !data_dir.starts_with(fixture_root) {
        anyhow::bail!("native mail-safety fixture data directory must be inside its fixture root");
    }
    if std::fs::read_to_string(data_dir.join(".dakia-native-mail-safety-profile"))
        .ok()
        .as_deref()
        != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native mail-safety fixture profile lacks its exact ownership sentinel");
    }
    Ok(data_dir)
}

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_loopback_endpoint(value: &str) -> bool {
    use std::net::IpAddr;

    value.eq_ignore_ascii_case("localhost")
        || value
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_port(value: &str, protocol: &str) -> anyhow::Result<u16> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| anyhow::anyhow!("native mail-safety {protocol} port is invalid"))
}

#[cfg(feature = "native-mail-safety-fixture")]
#[derive(Clone, Copy)]
struct NativeMailSafetyFixtureEndpoints<'a> {
    imap_host: &'a str,
    imap_port: u16,
    smtp_host: &'a str,
    smtp_port: u16,
}

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_account(
    id: &str,
    email: &str,
    account_name: &str,
    display_name: &str,
    endpoints: NativeMailSafetyFixtureEndpoints<'_>,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Account {
    Account {
        id: Uuid::parse_str(id).expect("native fixture account UUID is valid"),
        email: email.to_owned(),
        account_name: account_name.into(),
        display_name: display_name.into(),
        provider_id: "custom".into(),
        auth: AccountAuth::Password {
            username: email.to_owned(),
        },
        imap_host: endpoints.imap_host.to_owned(),
        imap_port: endpoints.imap_port,
        imap_security: provider::Security::Tls,
        smtp_host: endpoints.smtp_host.to_owned(),
        smtp_port: endpoints.smtp_port,
        smtp_security: provider::Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        // Setup intentionally skips automatic watchers in this mode. The
        // mounted production frontend starts them after its listeners exist.
        enabled: true,
        created_at,
    }
}

#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_account_matches(actual: &Account, expected: &Account) -> bool {
    actual.id == expected.id
        && actual.email == expected.email
        && actual.account_name == expected.account_name
        && actual.display_name == expected.display_name
        && actual.provider_id == expected.provider_id
        && matches!(
            (&actual.auth, &expected.auth),
            (
                AccountAuth::Password { username: actual },
                AccountAuth::Password { username: expected }
            ) if actual == expected
        )
        && actual.imap_host == expected.imap_host
        && actual.imap_port == expected.imap_port
        && actual.imap_security == provider::Security::Tls
        && actual.smtp_host == expected.smtp_host
        && actual.smtp_port == expected.smtp_port
        && actual.smtp_security == provider::Security::Tls
        && actual.archive_mailbox == expected.archive_mailbox
        && actual.spam_mailbox == expected.spam_mailbox
        && actual.enabled == expected.enabled
}

/// This profile exists solely for the opt-in native mail-safety acceptance
/// build. It is deliberately absent from ordinary and release binaries. The
/// runner supplies a fresh absolute directory, one or two fixed fictional
/// localhost accounts, and the one-run CA which the core feature separately
/// validates.
#[cfg(feature = "native-mail-safety-fixture")]
#[derive(Clone)]
struct NativeMailSafetyFixture {
    data_dir: PathBuf,
    account: Account,
    second_account: Option<Account>,
    password: String,
}

#[cfg(all(test, feature = "native-mail-safety-fixture"))]
fn native_mail_safety_unsubscribe_transport(
) -> anyhow::Result<Option<NativeMailSafetyOneClickTransport>> {
    use std::{
        env, fs,
        net::{IpAddr, Ipv4Addr, SocketAddr},
    };

    const VARIABLES: [&str; 4] = [
        "DAKIA_UNSUBSCRIBE_COMMAND_TLS_ROOT",
        "DAKIA_UNSUBSCRIBE_COMMAND_TLS_NONCE",
        "DAKIA_UNSUBSCRIBE_COMMAND_TLS_PORT",
        "DAKIA_UNSUBSCRIBE_COMMAND_TLS_CA_DER_PATH",
    ];
    let values = VARIABLES.map(env::var);
    if values
        .iter()
        .all(|value| matches!(value, Err(env::VarError::NotPresent)))
    {
        return Ok(None);
    }
    let required = |index: usize| -> anyhow::Result<&str> {
        values[index].as_deref().map_err(|_| {
            anyhow::anyhow!(
                "native unsubscribe command TLS fixture requires {}",
                VARIABLES[index]
            )
        })
    };
    let root = PathBuf::from(required(0)?);
    if !root.is_absolute() {
        anyhow::bail!("native unsubscribe command TLS fixture root must be absolute");
    }
    let root = root.canonicalize()?;
    let fixture_root = PathBuf::from(env::var("DAKIA_NATIVE_MAIL_SAFETY_ROOT")?).canonicalize()?;
    if root != fixture_root
        || fs::read_to_string(root.join(".dakia-native-mail-safety-root"))
            .ok()
            .as_deref()
            != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native unsubscribe command TLS fixture root is not owned");
    }
    let nonce = Uuid::parse_str(required(1)?)?;
    if nonce.to_string() != required(1)?
        || fs::read_to_string(root.join(".dakia-unsubscribe-command-tls"))
            .ok()
            .as_deref()
            != Some(&format!("{nonce}\n"))
    {
        anyhow::bail!("native unsubscribe command TLS fixture ownership marker is invalid");
    }
    let port = native_mail_safety_port(required(2)?, "unsubscribe")?;
    let ca_path = PathBuf::from(required(3)?);
    if !ca_path.is_absolute() || !ca_path.is_file() {
        anyhow::bail!("native unsubscribe command TLS fixture CA must be an absolute file");
    }
    let ca_path = ca_path.canonicalize()?;
    if !ca_path.starts_with(&root) {
        anyhow::bail!("native unsubscribe command TLS fixture CA must be inside its fixture root");
    }
    let endpoint = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    NativeMailSafetyOneClickTransport::new(endpoint, &fs::read(ca_path)?).map(Some)
}

async fn unsubscribe_with_command_transport(
    service: &MailService,
    message: &MailSummary,
) -> anyhow::Result<UnsubscribeOutcome> {
    #[cfg(all(test, feature = "native-mail-safety-fixture"))]
    if let Some(fixture) = native_mail_safety_unsubscribe_transport()? {
        return service
            .unsubscribe_with_native_mail_safety_fixture(message, &fixture)
            .await;
    }
    service.unsubscribe(message).await
}

#[cfg(feature = "native-mail-safety-fixture")]
impl NativeMailSafetyFixture {
    fn from_environment() -> anyhow::Result<Option<Self>> {
        use std::env;

        match env::var("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE") {
            Ok(value) if value == "1" => {}
            Ok(_) => anyhow::bail!("native mail-safety fixture marker must equal 1"),
            Err(env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let second_account = native_mail_safety_second_account_enabled(env::var(
            "DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT",
        ))?;
        let required = |name: &str| {
            env::var(name)
                .map_err(|_| anyhow::anyhow!("{name} is required for native mail-safety fixture"))
        };
        let fixture_root = PathBuf::from(required("DAKIA_NATIVE_MAIL_SAFETY_ROOT")?);
        if !fixture_root.is_absolute() {
            anyhow::bail!("native mail-safety fixture root must be absolute");
        }
        let fixture_root = fixture_root.canonicalize().map_err(|error| {
            anyhow::anyhow!("canonicalize native mail-safety fixture root: {error}")
        })?;
        if std::fs::read_to_string(fixture_root.join(".dakia-native-mail-safety-root"))
            .ok()
            .as_deref()
            != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
        {
            anyhow::bail!("native mail-safety fixture root lacks its exact ownership sentinel");
        }
        let required = |name: &str| {
            env::var(name)
                .map_err(|_| anyhow::anyhow!("{name} is required for native mail-safety fixture"))
        };
        let data_dir = native_mail_safety_profile_directory(
            &fixture_root,
            PathBuf::from(required("DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR")?),
        )?;
        // This happens before Store::open can create or migrate SQLite. A pre-existing
        // database is acceptable only inside the explicitly marked fixture profile.
        let database = data_dir.join("dakia.db");
        if database.exists()
            && std::fs::read_to_string(data_dir.join(".dakia-native-mail-safety-profile"))
                .ok()
                .as_deref()
                != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
        {
            anyhow::bail!("refusing to open an unowned native mail-safety fixture database");
        }
        let downloads_dir = PathBuf::from(required("DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR")?);
        if !downloads_dir.is_absolute() || !downloads_dir.is_dir() {
            anyhow::bail!(
                "native mail-safety fixture Downloads directory must be an existing absolute directory"
            );
        }
        let downloads_dir = downloads_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!("canonicalize native mail-safety fixture Downloads directory: {error}")
        })?;
        if !downloads_dir.starts_with(&fixture_root) {
            anyhow::bail!(
                "native mail-safety fixture Downloads directory must be inside its fixture root"
            );
        }
        let imap_host = required("DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST")?;
        let imap_port =
            native_mail_safety_port(&required("DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT")?, "IMAP")?;
        if !native_mail_safety_loopback_endpoint(&imap_host) {
            anyhow::bail!("native mail-safety fixture IMAP host must be loopback");
        }
        let smtp_host = required("DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST")?;
        let smtp_port =
            native_mail_safety_port(&required("DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT")?, "SMTP")?;
        let smtp_server_name = required("DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME")?;
        if !native_mail_safety_loopback_endpoint(&smtp_host)
            || !native_mail_safety_loopback_endpoint(&smtp_server_name)
        {
            anyhow::bail!("native mail-safety fixture SMTP endpoint must be loopback");
        }
        let ca_path = PathBuf::from(required("DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH")?);
        if !ca_path.is_absolute() || !ca_path.is_file() {
            anyhow::bail!("native mail-safety fixture CA must be an existing absolute file");
        }
        let ca_path = ca_path.canonicalize().map_err(|error| {
            anyhow::anyhow!("canonicalize native mail-safety fixture CA: {error}")
        })?;
        if !ca_path.starts_with(&fixture_root) {
            anyhow::bail!("native mail-safety fixture CA must be inside its fixture root");
        }
        let password = required("DAKIA_NATIVE_MAIL_SAFETY_PASSWORD")?;
        if password != "synthetic-only-password" {
            anyhow::bail!("native mail-safety fixture accepts its synthetic password only");
        }
        let created_at = chrono::Utc::now();
        let endpoints = NativeMailSafetyFixtureEndpoints {
            imap_host: &imap_host,
            imap_port,
            smtp_host: &smtp_host,
            smtp_port,
        };
        let account = native_mail_safety_account(
            NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_ID,
            NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_EMAIL,
            "Fictional native mail safety",
            "Fictional Reader",
            endpoints,
            created_at,
        );
        let second_account = second_account.then(|| {
            native_mail_safety_account(
                NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_ID,
                NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_EMAIL,
                "Fictional native mail safety two",
                "Fictional Reader Two",
                endpoints,
                created_at,
            )
        });
        Ok(Some(Self {
            data_dir,
            account,
            second_account,
            password,
        }))
    }

    fn accounts(&self) -> impl Iterator<Item = &Account> {
        std::iter::once(&self.account).chain(self.second_account.iter())
    }

    async fn seed_accounts(&self, store: &Store) -> anyhow::Result<()> {
        let existing_accounts = store.accounts().await?;
        let expected_count = usize::from(self.second_account.is_some()) + 1;
        if existing_accounts.len() > expected_count
            || existing_accounts.iter().any(|existing| {
                !self.accounts().any(|expected| {
                    existing.id == expected.id
                        && native_mail_safety_account_matches(existing, expected)
                })
            })
        {
            anyhow::bail!(
                "native mail-safety fixture data directory contains a non-fixture account"
            );
        }
        for expected in self.accounts() {
            if !existing_accounts
                .iter()
                .any(|existing| existing.id == expected.id)
            {
                store.save_account(expected).await?;
            }
        }
        Ok(())
    }

    async fn seed(&self, store: &Store) -> anyhow::Result<()> {
        self.seed_accounts(store).await?;
        let mail_service = MailService::new(store.clone());
        let credentials = mail_service.credentials();
        for account in self.accounts() {
            credentials.set_password(account, &self.password).await?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "native-mail-safety-fixture"))]
mod native_mail_safety_fixture_tests {
    use super::*;
    use std::{env::VarError, fs};
    use tempfile::tempdir;

    fn fixture_accounts(second_account: bool) -> (Account, Option<Account>) {
        let created_at = chrono::Utc::now();
        let endpoints = NativeMailSafetyFixtureEndpoints {
            imap_host: "127.0.0.1",
            imap_port: 3993,
            smtp_host: "127.0.0.1",
            smtp_port: 3465,
        };
        let account = native_mail_safety_account(
            NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_ID,
            NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_EMAIL,
            "Fictional native mail safety",
            "Fictional Reader",
            endpoints,
            created_at,
        );
        let second_account = second_account.then(|| {
            native_mail_safety_account(
                NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_ID,
                NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_EMAIL,
                "Fictional native mail safety two",
                "Fictional Reader Two",
                endpoints,
                created_at,
            )
        });
        (account, second_account)
    }

    #[test]
    fn second_account_requires_the_exact_opt_in_marker() {
        assert!(!native_mail_safety_second_account_enabled(Err(VarError::NotPresent)).unwrap());
        assert!(native_mail_safety_second_account_enabled(Ok("1".into())).unwrap());
        for value in ["", "0", "true", "01", "2"] {
            assert!(native_mail_safety_second_account_enabled(Ok(value.into())).is_err());
        }
    }

    #[test]
    fn fixture_endpoints_accept_loopback_only_and_seed_exact_smtp_tls_targets() {
        for endpoint in ["localhost", "LOCALHOST", "127.0.0.1", "::1"] {
            assert!(native_mail_safety_loopback_endpoint(endpoint));
        }
        for endpoint in ["", "example.test", "192.0.2.1", "8.8.8.8"] {
            assert!(!native_mail_safety_loopback_endpoint(endpoint));
        }
        for protocol in ["IMAP", "SMTP"] {
            assert_eq!(native_mail_safety_port("1", protocol).unwrap(), 1);
            assert_eq!(native_mail_safety_port("65535", protocol).unwrap(), 65535);
            for invalid in ["", "0", "65536", "not-a-port"] {
                assert!(native_mail_safety_port(invalid, protocol).is_err());
            }
        }

        let (primary, second) = fixture_accounts(true);
        for account in [primary, second.unwrap()] {
            assert_eq!(account.imap_host, "127.0.0.1");
            assert_eq!(account.imap_port, 3993);
            assert_eq!(account.imap_security, provider::Security::Tls);
            assert_eq!(account.smtp_host, "127.0.0.1");
            assert_eq!(account.smtp_port, 3465);
            assert_eq!(account.smtp_security, provider::Security::Tls);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn data_store_identifier_requires_the_exact_fixture_and_random_uuid() {
        let identifier = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x46, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert!(
            validate_native_mail_safety_data_store_identifier(None, false, Some(identifier))
                .is_err()
        );
        assert_eq!(
            validate_native_mail_safety_data_store_identifier(Some("1"), true, None).unwrap(),
            None
        );
        assert_eq!(
            validate_native_mail_safety_data_store_identifier(Some("1"), false, None).unwrap(),
            None
        );
        assert_eq!(
            validate_native_mail_safety_data_store_identifier(Some("1"), true, Some(identifier))
                .unwrap(),
            Some(identifier)
        );
        assert!(validate_native_mail_safety_data_store_identifier(
            Some("true"),
            true,
            Some(identifier)
        )
        .is_err());
        assert!(validate_native_mail_safety_data_store_identifier(
            Some("1"),
            false,
            Some(identifier)
        )
        .is_err());
        assert!(
            validate_native_mail_safety_data_store_identifier(Some("1"), true, Some([0; 16]))
                .is_err()
        );
        assert_eq!(
            parse_native_mail_safety_data_store_identifier("00112233-4455-4677-8899-aabbccddeeff")
                .unwrap(),
            identifier
        );
        for invalid in [
            "00112233-4455-4677-8899-AABBCCDDEEFF",
            "00112233445546778899aabbccddeeff",
            "not-a-uuid",
        ] {
            assert!(parse_native_mail_safety_data_store_identifier(invalid).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn data_store_identifier_is_injected_into_exactly_one_main_window() {
        let identifier = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x46, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut windows = vec![tauri::utils::config::WindowConfig {
            label: "main".into(),
            ..Default::default()
        }];
        set_native_mail_safety_main_data_store_identifier(&mut windows, Some(identifier)).unwrap();
        assert_eq!(windows[0].data_store_identifier, Some(identifier));
        assert!(
            set_native_mail_safety_main_data_store_identifier(&mut windows, Some(identifier))
                .is_err()
        );

        let mut static_identifier = vec![tauri::utils::config::WindowConfig {
            label: "main".into(),
            data_store_identifier: Some(identifier),
            ..Default::default()
        }];
        assert!(
            set_native_mail_safety_main_data_store_identifier(&mut static_identifier, None)
                .is_err()
        );

        let mut unconfigured = vec![tauri::utils::config::WindowConfig {
            label: "main".into(),
            ..Default::default()
        }];
        set_native_mail_safety_main_data_store_identifier(&mut unconfigured, None).unwrap();
        assert_eq!(unconfigured[0].data_store_identifier, None);

        let mut missing = vec![tauri::utils::config::WindowConfig {
            label: "reader".into(),
            ..Default::default()
        }];
        assert!(
            set_native_mail_safety_main_data_store_identifier(&mut missing, Some(identifier))
                .is_err()
        );

        let mut duplicate = vec![
            tauri::utils::config::WindowConfig {
                label: "main".into(),
                ..Default::default()
            },
            tauri::utils::config::WindowConfig {
                label: "main".into(),
                ..Default::default()
            },
        ];
        assert!(set_native_mail_safety_main_data_store_identifier(
            &mut duplicate,
            Some(identifier)
        )
        .is_err());
    }

    #[test]
    fn profile_directory_requires_an_exact_owned_child_sentinel() {
        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let profile = root.join("profile");
        fs::create_dir(&profile).unwrap();

        let missing = native_mail_safety_profile_directory(&root, profile.clone()).unwrap_err();
        assert!(missing.to_string().contains("ownership sentinel"));

        fs::write(
            profile.join(".dakia-native-mail-safety-profile"),
            NATIVE_MAIL_SAFETY_FIXTURE_MARKER,
        )
        .unwrap();
        assert_eq!(
            native_mail_safety_profile_directory(&root, profile.clone()).unwrap(),
            profile.canonicalize().unwrap()
        );

        fs::write(
            profile.join(".dakia-native-mail-safety-profile"),
            "similar-but-unowned\n",
        )
        .unwrap();
        assert!(native_mail_safety_profile_directory(&root, profile).is_err());
        assert!(native_mail_safety_profile_directory(&root, root.clone()).is_err());
    }

    #[tokio::test]
    async fn seed_adds_only_the_two_fixed_accounts_and_rejects_profile_contamination() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("dakia.db"))
            .await
            .unwrap();
        let (account, second_account) = fixture_accounts(true);
        let fixture = NativeMailSafetyFixture {
            data_dir: directory.path().to_owned(),
            account,
            second_account,
            password: "synthetic-only-password".into(),
        };

        fixture.seed_accounts(&store).await.unwrap();
        let accounts = store.accounts().await.unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|account| {
            account.id == Uuid::parse_str(NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_ID).unwrap()
                && account.email == NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_EMAIL
        }));
        assert!(accounts.iter().any(|account| {
            account.id == Uuid::parse_str(NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_ID).unwrap()
                && account.email == NATIVE_MAIL_SAFETY_SECOND_ACCOUNT_EMAIL
        }));

        let (account, second_account) = fixture_accounts(false);
        let single_account_fixture = NativeMailSafetyFixture {
            data_dir: directory.path().to_owned(),
            account,
            second_account,
            password: "synthetic-only-password".into(),
        };
        assert!(single_account_fixture.seed_accounts(&store).await.is_err());

        let mut contaminated = fixture.second_account.clone().unwrap();
        contaminated.email = "other@localhost".into();
        store.save_account(&contaminated).await.unwrap();
        assert!(fixture.seed_accounts(&store).await.is_err());

        let mut contaminated = fixture.second_account.clone().unwrap();
        contaminated.provider_id = "gmail".into();
        store.save_account(&contaminated).await.unwrap();
        assert!(fixture.seed_accounts(&store).await.is_err());

        let mut contaminated = fixture.second_account.clone().unwrap();
        contaminated.auth = AccountAuth::Password {
            username: NATIVE_MAIL_SAFETY_PRIMARY_ACCOUNT_EMAIL.into(),
        };
        store.save_account(&contaminated).await.unwrap();
        assert!(fixture.seed_accounts(&store).await.is_err());
    }
}

struct AppState {
    store: Store,
    data_dir: PathBuf,
    classifier: Mutex<Box<dyn EmailClassifier>>,
    classification_owner: String,
    classification: Arc<ClassificationScheduler>,
    realtime: RealtimeSyncManager,
    remote_operation_slots: Arc<Semaphore>,
    mail_rebuilds: Mutex<HashMap<Uuid, MailRebuildProgress>>,
    mail_rebuild_running: Mutex<HashSet<Uuid>>,
    mail_rebuild_cancellations: MailRebuildCancellations,
    account_operations: AccountOperationLocks,
    translation_downloads: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

trait EmailClassifier: Send {
    fn revision(&self) -> &str {
        "test-classifier"
    }

    fn classify(
        &mut self,
        emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>>;
}

impl EmailClassifier for LocalEmailClassifier {
    fn revision(&self) -> &str {
        LocalEmailClassifier::revision(self)
    }

    fn classify(
        &mut self,
        emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        LocalEmailClassifier::classify(self, emails)
    }
}

/// Serializes destructive and provider-backed work for one account without
/// unnecessarily blocking other accounts.  A deletion holds this lock from
/// stopping realtime through the storage transaction, so an in-flight manual
/// sync or resumed rebuild cannot finish by repopulating the removed account.
/// Coordinates a rebuild cancellation across the brief interval before a
/// queued rebuild obtains the per-account operation lock.
#[derive(Default)]
struct MailRebuildCancellations {
    active: Mutex<HashMap<Uuid, MailRebuildCancellation>>,
}

struct MailRebuildCancellation {
    sender: watch::Sender<MailRebuildCancellationDisposition>,
    registrations: usize,
    reserved: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MailRebuildCancellationDisposition {
    None,
    Retain,
    Replace,
    Remove,
}

impl MailRebuildCancellations {
    fn reserve(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        let cancellation = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(MailRebuildCancellationDisposition::None);
            MailRebuildCancellation {
                sender,
                registrations: 0,
                reserved: false,
            }
        });
        cancellation.reserved = true;
    }

    fn register(&self, account_id: Uuid) -> watch::Receiver<MailRebuildCancellationDisposition> {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        let cancellation = active.entry(account_id).or_insert_with(|| {
            let (sender, _) = watch::channel(MailRebuildCancellationDisposition::None);
            MailRebuildCancellation {
                sender,
                registrations: 0,
                reserved: false,
            }
        });
        cancellation.registrations += 1;
        cancellation.sender.subscribe()
    }

    fn request(&self, account_id: Uuid, disposition: MailRebuildCancellationDisposition) {
        if let Some(sender) = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned")
            .get(&account_id)
            .map(|cancellation| cancellation.sender.clone())
        {
            let current = *sender.borrow();
            let next = match (current, disposition) {
                (MailRebuildCancellationDisposition::Remove, _)
                | (_, MailRebuildCancellationDisposition::Remove) => {
                    MailRebuildCancellationDisposition::Remove
                }
                (MailRebuildCancellationDisposition::Replace, _)
                | (_, MailRebuildCancellationDisposition::Replace) => {
                    MailRebuildCancellationDisposition::Replace
                }
                (MailRebuildCancellationDisposition::Retain, _)
                | (_, MailRebuildCancellationDisposition::Retain) => {
                    MailRebuildCancellationDisposition::Retain
                }
                _ => MailRebuildCancellationDisposition::None,
            };
            // `send` drops the update when a reservation has no subscribed
            // task yet. `send_replace` records it for the later register.
            sender.send_replace(next);
        }
    }

    fn disposition(
        &self,
        receiver: &watch::Receiver<MailRebuildCancellationDisposition>,
    ) -> MailRebuildCancellationDisposition {
        *receiver.borrow()
    }

    fn clear(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        if let Some(cancellation) = active.get_mut(&account_id) {
            cancellation.registrations -= 1;
            if cancellation.registrations == 0 && !cancellation.reserved {
                active.remove(&account_id);
            }
        }
    }

    fn release_reservation(&self, account_id: Uuid) {
        let mut active = self
            .active
            .lock()
            .expect("mail rebuild cancellation lock poisoned");
        if let Some(cancellation) = active.get_mut(&account_id) {
            cancellation.reserved = false;
            if cancellation.registrations == 0 {
                active.remove(&account_id);
            }
        }
    }
}

fn normalized_account_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn matching_account_email(account: &Account, email: &str) -> bool {
    normalized_account_email(&account.email) == normalized_account_email(email)
}

fn same_mail_namespace(existing: &Account, candidate: &Account) -> bool {
    existing.provider_id == candidate.provider_id
        && match (&existing.auth, &candidate.auth) {
            (
                AccountAuth::Password {
                    username: existing_username,
                },
                AccountAuth::Password {
                    username: candidate_username,
                },
            ) => existing_username == candidate_username,
            (
                AccountAuth::OAuth2 {
                    username: existing_username,
                    provider: existing_provider,
                    ..
                },
                AccountAuth::OAuth2 {
                    username: candidate_username,
                    provider: candidate_provider,
                    ..
                },
            ) => existing_username == candidate_username && existing_provider == candidate_provider,
            _ => false,
        }
        && existing
            .imap_host
            .trim()
            .eq_ignore_ascii_case(candidate.imap_host.trim())
        && existing.imap_port == candidate.imap_port
        && existing.imap_security == candidate.imap_security
        && existing.archive_mailbox == candidate.archive_mailbox
        && existing.spam_mailbox == candidate.spam_mailbox
}

fn credential_secret_name(account: &Account) -> String {
    // Keep this aligned with `dakia_core::mail::CredentialStore::key` so a
    // failed account save can restore a credential it just replaced.
    format!("dev.dakia.mail:{}:{}", account.id, account.auth.username())
}

/// Converts only the authentication scheme. Callers retain the same account
/// value so mailbox settings, ID, and indexed data remain associated with it.
fn convert_legacy_oauth_to_password(account: &mut Account) -> bool {
    let AccountAuth::OAuth2 { username, .. } = &account.auth else {
        return false;
    };
    account.auth = AccountAuth::Password {
        username: username.clone(),
    };
    true
}

fn validate_legacy_oauth_conversion(
    account: &Account,
    statuses: &[RealtimeSyncStatus],
) -> Result<(), String> {
    let AccountAuth::OAuth2 { provider, .. } = &account.auth else {
        return Ok(());
    };
    if account.provider_id != "gmail" || provider != "gmail" {
        return Err("Only legacy Gmail OAuth accounts can be converted to an app password".into());
    }
    let failed_authentication = statuses.iter().any(|status| {
        status.account_id == account.id
            && status.state == "paused"
            && status.error_kind.as_deref() == Some("authentication")
    });
    if !failed_authentication {
        return Err(
            "This Gmail account can use an app password after its existing sign-in stops working"
                .into(),
        );
    }
    Ok(())
}

fn ensure_account_is_not_connected(accounts: &[Account], email: &str) -> Result<(), String> {
    if accounts
        .iter()
        .any(|stored| matching_account_email(stored, email))
    {
        return Err("This account is already connected. Update it in Settings.".into());
    }
    Ok(())
}

#[cfg(test)]
mod legacy_oauth_conversion_tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn conversion_preserves_the_account_identity_and_credential_key() {
        let id = Uuid::new_v4();
        let mut account = Account {
            id,
            email: "legacy@gmail.com".into(),
            account_name: "Personal Gmail".into(),
            display_name: "Legacy User".into(),
            provider_id: "gmail".into(),
            auth: AccountAuth::OAuth2 {
                username: "legacy@gmail.com".into(),
                provider: "gmail".into(),
                access_token_expires_at: None,
            },
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::Security::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::Security::Tls,
            archive_mailbox: "[Gmail]/All Mail".into(),
            spam_mailbox: "[Gmail]/Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let previous_secret_name = credential_secret_name(&account);

        assert!(convert_legacy_oauth_to_password(&mut account));

        assert_eq!(account.id, id);
        assert_eq!(account.account_name, "Personal Gmail");
        assert_eq!(account.archive_mailbox, "[Gmail]/All Mail");
        assert_eq!(credential_secret_name(&account), previous_secret_name);
        assert!(matches!(account.auth, AccountAuth::Password { .. }));
    }

    #[test]
    fn password_accounts_are_not_converted() {
        let mut account = AccountDraft {
            email: "already-password@example.test".into(),
            display_name: "Password User".into(),
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

        assert!(!convert_legacy_oauth_to_password(&mut account));
        assert!(matches!(account.auth, AccountAuth::Password { .. }));
    }

    #[test]
    fn app_password_conversion_requires_a_paused_legacy_gmail_oauth_account() {
        let id = Uuid::new_v4();
        let mut gmail = Account {
            id,
            email: "legacy@gmail.com".into(),
            account_name: "Legacy Gmail".into(),
            display_name: "Legacy User".into(),
            provider_id: "gmail".into(),
            auth: AccountAuth::OAuth2 {
                username: "legacy@gmail.com".into(),
                provider: "gmail".into(),
                access_token_expires_at: None,
            },
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::Security::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::Security::Tls,
            archive_mailbox: "[Gmail]/All Mail".into(),
            spam_mailbox: "[Gmail]/Spam".into(),
            enabled: true,
            created_at: Utc::now(),
        };
        let paused_authentication = vec![RealtimeSyncStatus {
            account_id: id,
            state: "paused".into(),
            retry_at: None,
            error_kind: Some("authentication".into()),
        }];

        assert!(validate_legacy_oauth_conversion(&gmail, &paused_authentication).is_ok());
        assert!(validate_legacy_oauth_conversion(&gmail, &[])
            .unwrap_err()
            .contains("after its existing sign-in stops working"));

        gmail.provider_id = "outlook".into();
        gmail.auth = AccountAuth::OAuth2 {
            username: "legacy@gmail.com".into(),
            provider: "outlook".into(),
            access_token_expires_at: None,
        };
        assert!(
            validate_legacy_oauth_conversion(&gmail, &paused_authentication)
                .unwrap_err()
                .contains("Only legacy Gmail OAuth accounts")
        );
    }

    #[test]
    fn add_account_rejects_case_insensitive_duplicate_email() {
        let existing = AccountDraft {
            email: "already-connected@example.test".into(),
            display_name: "Connected User".into(),
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

        assert!(
            ensure_account_is_not_connected(&[existing], " ALREADY-CONNECTED@example.test ")
                .unwrap_err()
                .contains("already connected")
        );
    }
}

async fn enabled_account_for_operation(
    state: &AppState,
    account_id: Uuid,
) -> Result<Account, String> {
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    if !account.enabled {
        return Err("Account is disabled".to_owned());
    }
    Ok(account)
}

async fn restart_realtime_if_current(
    app: tauri::AppHandle,
    state: &Arc<AppState>,
    account_id: Uuid,
) -> anyhow::Result<()> {
    // Never use an account snapshot captured before provider work: it may
    // have been removed or disabled while that work was finishing.
    if let Some(account) = state.store.account(account_id).await? {
        if account.enabled {
            state.realtime.start_account(app, account).await;
        }
    }
    Ok(())
}

fn complete_manual_sync_attempt<T>(
    refresh: anyhow::Result<T>,
    restart: anyhow::Result<()>,
    account_id: Uuid,
) -> anyhow::Result<T> {
    match (refresh, restart) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(restart_error)) => Err(restart_error),
        (Err(refresh_error), Ok(())) => Err(refresh_error),
        (Err(refresh_error), Err(restart_error)) => {
            // The caller asked to sync. Preserve that failure while still
            // recording that recovery also could not start.
            tracing::warn!(
                account_id = %account_id,
                error = %restart_error,
                "could not restart real-time mail after a failed manual sync"
            );
            Err(refresh_error)
        }
    }
}

#[cfg(test)]
mod account_operation_lock_tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: Uuid::new_v4(),
            email: "reader@example.com".into(),
            account_name: "Reader".into(),
            display_name: "Reader".into(),
            provider_id: "fastmail".into(),
            auth: AccountAuth::Password {
                username: "reader@example.com".into(),
            },
            imap_host: "imap.fastmail.com".into(),
            imap_port: 993,
            imap_security: dakia_core::provider::Security::Tls,
            smtp_host: "smtp.fastmail.com".into(),
            smtp_port: 465,
            smtp_security: dakia_core::provider::Security::Tls,
            archive_mailbox: "Archive".into(),
            spam_mailbox: "Spam".into(),
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn serializes_the_same_account_without_blocking_the_lock_registry() {
        let locks = Arc::new(AccountOperationLocks::default());
        let account_id = Uuid::new_v4();
        let first = locks.acquire(account_id).await;
        let waiting = locks.clone();
        let (acquired, mut receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _second = waiting.acquire(account_id).await;
            let _ = acquired.send(());
        });

        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut receiver)
            .await
            .expect("second operation should acquire after the first exits")
            .expect("operation task should report acquisition");
        task.await.expect("operation task should finish");
    }

    #[test]
    fn failed_refresh_remains_the_reported_error_when_recovery_also_fails() {
        let error = complete_manual_sync_attempt::<()>(
            Err(anyhow::anyhow!("refresh failed")),
            Err(anyhow::anyhow!("restart failed")),
            Uuid::nil(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "refresh failed");
    }

    #[test]
    fn restart_error_is_reported_after_a_successful_refresh() {
        let error = complete_manual_sync_attempt::<()>(
            Ok(()),
            Err(anyhow::anyhow!("restart failed")),
            Uuid::nil(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "restart failed");
    }

    #[tokio::test]
    async fn cancellation_reaches_a_rebuild_registered_while_waiting_for_its_account_lock() {
        let cancellations = MailRebuildCancellations::default();
        let account_id = Uuid::new_v4();
        let locks = AccountOperationLocks::default();
        let held_lock = locks.acquire(account_id).await;

        let receiver = cancellations.register(account_id);
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);

        assert_eq!(
            cancellations.disposition(&receiver),
            MailRebuildCancellationDisposition::Retain
        );
        drop(held_lock);
        cancellations.clear(account_id);

        // An account update with no registered rebuild must not turn into a
        // stale cancellation for a later, unrelated full sync.
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);
        let next_attempt = cancellations.register(account_id);
        assert_eq!(
            cancellations.disposition(&next_attempt),
            MailRebuildCancellationDisposition::None
        );
    }

    #[test]
    fn cancellation_reaches_a_reserved_rebuild_before_its_task_registers() {
        let cancellations = MailRebuildCancellations::default();
        let account_id = Uuid::new_v4();

        cancellations.reserve(account_id);
        cancellations.request(account_id, MailRebuildCancellationDisposition::Retain);
        let receiver = cancellations.register(account_id);

        assert_eq!(
            cancellations.disposition(&receiver),
            MailRebuildCancellationDisposition::Retain
        );
        cancellations.clear(account_id);
        cancellations.release_reservation(account_id);

        // A later unrelated reservation gets a fresh channel, rather than a
        // stale cancellation from a completed update.
        cancellations.reserve(account_id);
        let next = cancellations.register(account_id);
        assert_eq!(
            cancellations.disposition(&next),
            MailRebuildCancellationDisposition::None
        );
    }

    #[test]
    fn durable_reset_intent_cannot_be_downgraded_by_a_queued_worker() {
        assert!(effective_rebuild_reset(false, Some(true)));
        assert!(effective_rebuild_reset(true, Some(false)));
        assert!(!effective_rebuild_reset(false, Some(false)));
    }

    #[test]
    fn initial_reset_jobs_survive_missing_or_rejected_credentials() {
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("mail rebuild cancelled"),
            MailRebuildCancellationDisposition::Remove,
            true,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP authentication rejected: invalid credentials"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP authentication rejected: invalid credentials"),
            MailRebuildCancellationDisposition::None,
            true,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &anyhow::anyhow!("credentials are not stored for this account"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("credentials are not stored for this account"),
            MailRebuildCancellationDisposition::None,
            true,
        ));
        assert!(should_retain_mail_rebuild_job(
            &anyhow::anyhow!("IMAP connection closed during command"),
            MailRebuildCancellationDisposition::None,
            false,
        ));
    }

    #[test]
    fn cancellation_disposition_retains_replacement_but_removes_deleted_account_jobs() {
        let transient = anyhow::anyhow!("IMAP connection closed during command");
        let permanent = anyhow::anyhow!("IMAP authentication rejected");
        assert!(should_retain_mail_rebuild_job(
            &permanent,
            MailRebuildCancellationDisposition::Retain,
            false,
        ));
        assert!(should_retain_mail_rebuild_job(
            &transient,
            MailRebuildCancellationDisposition::Replace,
            false,
        ));
        assert!(!should_retain_mail_rebuild_job(
            &transient,
            MailRebuildCancellationDisposition::Remove,
            true,
        ));
    }

    #[test]
    fn normalizes_email_identity_for_account_reuse() {
        assert_eq!(
            normalized_account_email(" Existing@Example.Com "),
            "existing@example.com"
        );
    }

    #[test]
    fn reuses_an_index_only_for_the_same_remote_namespace() {
        let existing = account();
        let same_remote = existing.clone();
        assert!(same_mail_namespace(&existing, &same_remote));

        let mut different_provider = same_remote.clone();
        different_provider.provider_id = "outlook".into();
        assert!(!same_mail_namespace(&existing, &different_provider));

        let mut different_host = same_remote.clone();
        different_host.imap_host = "imap.other.example".into();
        assert!(!same_mail_namespace(&existing, &different_host));

        let mut different_username = same_remote.clone();
        different_username.auth = AccountAuth::Password {
            username: "other@example.com".into(),
        };
        assert!(!same_mail_namespace(&existing, &different_username));

        let mut different_mailbox = same_remote;
        different_mailbox.archive_mailbox = "All Mail".into();
        assert!(!same_mail_namespace(&existing, &different_mailbox));
    }
}
#[derive(Default)]
struct ClassificationScheduleState {
    running: bool,
    requested_generation: u64,
    completed_generation: u64,
    last_completed_count: usize,
    last_failure: Option<(u64, String)>,
}

#[derive(Default)]
struct ClassificationScheduler {
    state: Mutex<ClassificationScheduleState>,
    completed: Notify,
}

impl ClassificationScheduler {
    /// Coalesce repeated native kicks into one runner while retaining a
    /// generation for every request that arrives during a drain.
    fn request(&self) -> (u64, bool) {
        let mut state = self.state.lock().expect("classification lock poisoned");
        state.requested_generation += 1;
        let generation = state.requested_generation;
        let should_start = !state.running;
        if should_start {
            state.running = true;
        }
        (generation, should_start)
    }

    fn next_generation(&self) -> u64 {
        self.state
            .lock()
            .expect("classification lock poisoned")
            .requested_generation
    }

    /// Returns true when a kick arrived while this pass was running, so the
    /// current runner must make another database pass before it may stop.
    fn finish_generation(&self, generation: u64, classified: usize) -> bool {
        let mut state = self.state.lock().expect("classification lock poisoned");
        if state.requested_generation > generation {
            return true;
        }
        state.completed_generation = generation;
        state.last_completed_count = classified;
        state.last_failure = None;
        state.running = false;
        self.completed.notify_waiters();
        false
    }

    /// Returns true when a newer kick must be retried after a failed pass.
    fn fail_generation(&self, generation: u64, error: String) -> bool {
        let mut state = self.state.lock().expect("classification lock poisoned");
        state.completed_generation = generation;
        state.last_failure = Some((generation, error));
        let retry = state.requested_generation > generation;
        state.running = retry;
        self.completed.notify_waiters();
        retry
    }

    async fn wait_for(&self, generation: u64) -> anyhow::Result<usize> {
        loop {
            let notified = self.completed.notified();
            let completed = {
                let state = self.state.lock().expect("classification lock poisoned");
                if state.completed_generation < generation {
                    None
                } else if let Some((failed_generation, failure)) = &state.last_failure {
                    if *failed_generation >= generation {
                        Some(Err(anyhow::anyhow!(failure.clone())))
                    } else {
                        Some(Ok(state.last_completed_count))
                    }
                } else {
                    Some(Ok(state.last_completed_count))
                }
            };
            if let Some(result) = completed {
                return result;
            }
            notified.await;
        }
    }
}

fn classification_retry_delay(consecutive_failures: usize) -> Option<Duration> {
    CLASSIFICATION_RETRY_DELAYS
        .get(consecutive_failures.saturating_sub(1))
        .copied()
}

fn validate_classification_output_count(
    input_count: usize,
    output_count: usize,
) -> anyhow::Result<()> {
    if input_count != output_count {
        return Err(anyhow::anyhow!(
            "classifier returned {output_count} results for {input_count} messages"
        ));
    }
    Ok(())
}

fn validate_classification_apply_count(attempted: usize, applied: usize) -> anyhow::Result<usize> {
    if attempted != applied {
        anyhow::bail!(
            "classification batch became stale ({applied}/{attempted} results applied); retrying with current evidence"
        );
    }
    Ok(applied)
}

async fn retry_classification_batch<T, F, Fut>(mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut failures = 0;
    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(error) => {
                failures += 1;
                let Some(delay) = classification_retry_delay(failures) else {
                    return Err(error);
                };
                tracing::warn!(
                    attempt = failures,
                    retry_delay_ms = delay.as_millis(),
                    error = %error,
                    "classification batch failed; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod classification_scheduler_tests {
    use super::*;

    #[test]
    fn coalesces_kicks_without_losing_one_that_arrives_during_a_pass() {
        let scheduler = ClassificationScheduler::default();
        let (first, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let (second, starts_second_runner) = scheduler.request();
        assert!(!starts_second_runner);
        assert!(scheduler.finish_generation(first, 64));
        assert_eq!(scheduler.next_generation(), second);
        assert!(!scheduler.finish_generation(second, 65));
    }

    #[tokio::test]
    async fn manual_waiter_observes_the_serialized_drain_result() {
        let scheduler = Arc::new(ClassificationScheduler::default());
        let (generation, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.wait_for(generation).await })
        };
        tokio::task::yield_now().await;
        assert!(!scheduler.finish_generation(generation, 7));
        assert_eq!(waiter.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn failed_drain_wakes_waiters_and_a_later_kick_starts_a_new_drain() {
        let scheduler = Arc::new(ClassificationScheduler::default());
        let (failed_generation, starts_runner) = scheduler.request();
        assert!(starts_runner);
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.wait_for(failed_generation).await })
        };
        tokio::task::yield_now().await;
        assert!(!scheduler.fail_generation(failed_generation, "classifier unavailable".into()));
        assert_eq!(
            waiter.await.unwrap().unwrap_err().to_string(),
            "classifier unavailable"
        );

        let (recovery_generation, starts_recovery) = scheduler.request();
        assert!(starts_recovery);
        assert!(!scheduler.finish_generation(recovery_generation, 4));
        assert_eq!(scheduler.wait_for(recovery_generation).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn retries_a_transient_batch_failure_without_an_external_kick() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let classified = retry_classification_batch({
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Err(anyhow::anyhow!("temporary classifier failure"))
                    } else {
                        Ok(6)
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(classified, 6);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_policy_has_two_bounded_backoff_delays() {
        assert_eq!(
            classification_retry_delay(1),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            classification_retry_delay(2),
            Some(Duration::from_millis(500))
        );
        assert_eq!(classification_retry_delay(3), None);
    }

    #[test]
    fn rejects_classifier_output_that_does_not_cover_the_entire_batch() {
        assert!(validate_classification_output_count(3, 3).is_ok());
        assert_eq!(
            validate_classification_output_count(3, 2)
                .unwrap_err()
                .to_string(),
            "classifier returned 2 results for 3 messages"
        );
        assert_eq!(
            validate_classification_output_count(3, 4)
                .unwrap_err()
                .to_string(),
            "classifier returned 4 results for 3 messages"
        );
    }

    #[test]
    fn stale_classification_apply_is_retryable_instead_of_counted_as_progress() {
        assert_eq!(validate_classification_apply_count(3, 3).unwrap(), 3);
        assert_eq!(
            validate_classification_apply_count(3, 0)
                .unwrap_err()
                .to_string(),
            "classification batch became stale (0/3 results applied); retrying with current evidence"
        );
    }
}

async fn run_bounded_ordered<T, U, F, Fut>(
    items: Vec<T>,
    max_in_flight: usize,
    limiter: Arc<Semaphore>,
    operation: F,
) -> Vec<U>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = U> + Send + 'static,
{
    assert!(max_in_flight > 0, "bounded work requires a non-zero limit");
    let mut pending = items.into_iter().enumerate();
    let mut active = tokio::task::JoinSet::new();
    let mut completed = Vec::new();

    loop {
        while active.len() < max_in_flight {
            let Some((index, item)) = pending.next() else {
                break;
            };
            let operation = operation.clone();
            let limiter = limiter.clone();
            active.spawn(async move {
                let _permit = limiter
                    .acquire_owned()
                    .await
                    .expect("shared operation limiter must remain open");
                (index, operation(item).await)
            });
        }
        let Some(joined) = active.join_next().await else {
            break;
        };
        completed.push(joined.expect("bounded task must not panic"));
    }

    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, output)| output).collect()
}

#[cfg(test)]
mod bounded_concurrency_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn caps_work_and_restores_input_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let outputs = run_bounded_ordered((0..12).collect(), 3, Arc::new(Semaphore::new(12)), {
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
                    index
                }
            }
        })
        .await;

        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(outputs, (0..12).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn keeps_failures_in_input_order() {
        let outputs = run_bounded_ordered(
            (0..4).collect(),
            4,
            Arc::new(Semaphore::new(4)),
            |index| async move {
                tokio::time::sleep(Duration::from_millis((4 - index) as u64)).await;
                if matches!(index, 1 | 3) {
                    Err(index)
                } else {
                    Ok(index)
                }
            },
        )
        .await;

        assert_eq!(outputs, vec![Ok(0), Err(1), Ok(2), Err(3)]);
        assert_eq!(outputs.into_iter().find_map(Result::err), Some(1));
    }

    #[tokio::test]
    async fn dropping_bounded_work_aborts_in_flight_tasks() {
        struct ActiveGuard(Arc<AtomicUsize>);
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let limiter = Arc::new(Semaphore::new(2));
        let task = tokio::spawn(run_bounded_ordered(
            (0..20).collect(),
            2,
            limiter.clone(),
            {
                let active = active.clone();
                move |_| {
                    let active = active.clone();
                    async move {
                        active.fetch_add(1, Ordering::SeqCst);
                        let _guard = ActiveGuard(active);
                        std::future::pending::<()>().await
                    }
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the bounded batch should start");

        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(limiter.available_permits(), 2);
    }

    #[tokio::test]
    async fn overlapping_invocations_share_the_application_limit() {
        let limiter = Arc::new(Semaphore::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let invoke = |offset| {
            let limiter = limiter.clone();
            let active = active.clone();
            let peak = peak.clone();
            tokio::spawn(run_bounded_ordered(
                (0..8).map(|index| offset + index).collect(),
                4,
                limiter,
                move |index| {
                    let active = active.clone();
                    let peak = peak.clone();
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        index
                    }
                },
            ))
        };

        let (left, right) = tokio::join!(invoke(0), invoke(100));
        assert_eq!(left.unwrap().len(), 8);
        assert_eq!(right.unwrap().len(), 8);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(limiter.available_permits(), 3);
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailRebuildProgress {
    account_id: Uuid,
    phase: String,
    completed: usize,
    total: Option<usize>,
    #[serde(skip)]
    reset_before_sync: bool,
}

impl From<MailRebuildJob> for MailRebuildProgress {
    fn from(job: MailRebuildJob) -> Self {
        Self {
            account_id: job.account_id,
            phase: job.phase,
            completed: job.completed,
            total: job.total,
            reset_before_sync: job.reset_before_sync,
        }
    }
}

const MAX_DROPPED_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_DROPPED_ATTACHMENT_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_DROPPED_ATTACHMENTS: usize = 50;
const DROPPED_FILE_RECEIPT_TTL: Duration = Duration::from_secs(30);
const DROPPED_FILE_RECEIPT_EVENT: &str = "dakia://dropped-file-receipt";
const DROPPED_FILE_ERROR_EVENT: &str = "dakia://dropped-file-error";
#[cfg(target_os = "macos")]
const TERMINAL_COMMAND_PATH: &str = "/usr/local/bin/dakia";

struct DroppedFileReceipt {
    window_label: String,
    files: Vec<OpenDroppedFile>,
    expires_at: Instant,
}

#[derive(Default)]
struct DroppedFileReceiptStore {
    entries: Mutex<HashMap<String, DroppedFileReceipt>>,
}

impl DroppedFileReceiptStore {
    fn issue(&self, window_label: &str, paths: Vec<PathBuf>) -> Result<String, String> {
        self.issue_at(window_label, paths, Instant::now())
    }

    fn issue_at(
        &self,
        window_label: &str,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> Result<String, String> {
        if paths.is_empty() {
            return Err("No files were dropped".into());
        }
        let files = open_dropped_files(paths)?;
        let receipt = Uuid::new_v4().to_string();
        let mut entries = self.entries.lock().map_err(error)?;
        entries.retain(|_, entry| entry.expires_at > now);
        entries.insert(
            receipt.clone(),
            DroppedFileReceipt {
                window_label: window_label.to_owned(),
                files,
                expires_at: now + DROPPED_FILE_RECEIPT_TTL,
            },
        );
        Ok(receipt)
    }

    fn consume(&self, receipt: &str, window_label: &str) -> Result<Vec<OpenDroppedFile>, String> {
        self.consume_at(receipt, window_label, Instant::now())
    }

    fn consume_at(
        &self,
        receipt: &str,
        window_label: &str,
        now: Instant,
    ) -> Result<Vec<OpenDroppedFile>, String> {
        let invalid = || "Dropped-file authorization is invalid or expired".to_owned();
        let mut entries = self.entries.lock().map_err(error)?;
        entries.retain(|_, entry| entry.expires_at > now);
        let entry = entries.get(receipt).ok_or_else(invalid)?;
        if entry.window_label != window_label {
            return Err(invalid());
        }
        Ok(entries
            .remove(receipt)
            .expect("receipt existed while the store lock was held")
            .files)
    }

    fn revoke_window(&self, window_label: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, entry| entry.window_label != window_label);
        }
    }

    fn expire_at(&self, receipt: &str, now: Instant) {
        if let Ok(mut entries) = self.entries.lock() {
            if entries
                .get(receipt)
                .is_some_and(|entry| entry.expires_at <= now)
            {
                entries.remove(receipt);
            }
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct DroppedAttachment {
    filename: String,
    mime_type: String,
    content_base64: String,
    size_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum TerminalCommandStatus {
    Available,
    NotSetUp,
    Conflict,
}

#[cfg(target_os = "macos")]
fn bundled_cli_path(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let path = std::env::current_exe()
        .map_err(error)?
        .parent()
        .ok_or_else(|| "Dakia could not locate its application directory.".to_string())?
        .join("dakia");
    if path.starts_with("/Volumes") {
        return Err(
            "Move Dakia to your Applications folder before setting up the terminal command.".into(),
        );
    }
    if path.is_file() {
        Ok(path)
    } else {
        Err("The Dakia terminal command is missing from this app installation.".into())
    }
}

#[cfg(target_os = "macos")]
fn terminal_command_status_for(source: &Path) -> TerminalCommandStatus {
    let destination = Path::new(TERMINAL_COMMAND_PATH);
    match std::fs::symlink_metadata(destination) {
        Err(error) if error.kind() == ErrorKind::NotFound => TerminalCommandStatus::NotSetUp,
        Err(_) => TerminalCommandStatus::Conflict,
        Ok(metadata) if !metadata.file_type().is_symlink() => TerminalCommandStatus::Conflict,
        Ok(_) => match std::fs::read_link(destination) {
            Ok(target) if target == source => TerminalCommandStatus::Available,
            _ => TerminalCommandStatus::Conflict,
        },
    }
}

#[cfg(target_os = "macos")]
fn set_terminal_menu_label(app: &tauri::AppHandle, status: &TerminalCommandStatus) {
    let Some(menu) = app.menu() else {
        return;
    };
    let Some(item) = menu.get("terminal-command") else {
        return;
    };
    let Some(item) = item.as_menuitem() else {
        return;
    };
    let label = match status {
        TerminalCommandStatus::Available => "Remove Dakia Terminal Command…",
        _ => "Use Dakia from Terminal…",
    };
    let _ = item.set_text(label);
}

#[cfg(target_os = "macos")]
fn run_privileged_terminal_command(script: &str, source: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "set sourcePath to item 1 of argv",
            "-e",
            script,
            "-e",
            "end run",
            "--",
        ])
        .arg(source)
        .output()
        .map_err(error)?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.contains("User canceled") {
            Err("Setup was canceled.".into())
        } else {
            Err(format!(
                "macOS could not update the terminal command. {detail}"
            ))
        }
    }
}

#[tauri::command]
fn terminal_command_status(app: tauri::AppHandle) -> Result<TerminalCommandStatus, String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        Ok(terminal_command_status_for(&source))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[tauri::command]
async fn install_terminal_command(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        match terminal_command_status_for(&source) {
            TerminalCommandStatus::Available => return Ok(()),
            TerminalCommandStatus::Conflict => {
                return Err(format!(
                    "Another item already exists at {TERMINAL_COMMAND_PATH}. It was left unchanged."
                ))
            }
            TerminalCommandStatus::NotSetUp => {}
        }
        let install_source = source.clone();
        tauri::async_runtime::spawn_blocking(move || {
            run_privileged_terminal_command(
                "do shell script \"/bin/mkdir -p /usr/local/bin && /bin/test ! -e /usr/local/bin/dakia && /bin/test ! -L /usr/local/bin/dakia && /bin/ln -s \" & quoted form of sourcePath & \" /usr/local/bin/dakia\" with administrator privileges",
                &install_source,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        let status = terminal_command_status_for(&source);
        if !matches!(status, TerminalCommandStatus::Available) {
            return Err("The terminal command could not be verified after setup.".into());
        }
        set_terminal_menu_label(&app, &status);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[tauri::command]
async fn remove_terminal_command(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let source = bundled_cli_path(&app)?;
        match terminal_command_status_for(&source) {
            TerminalCommandStatus::NotSetUp => return Ok(()),
            TerminalCommandStatus::Conflict => {
                return Err(format!(
                    "{TERMINAL_COMMAND_PATH} does not belong to this copy of Dakia and was left unchanged."
                ))
            }
            TerminalCommandStatus::Available => {}
        }
        let remove_source = source.clone();
        tauri::async_runtime::spawn_blocking(move || {
            run_privileged_terminal_command(
                "do shell script \"current=$(/usr/bin/readlink /usr/local/bin/dakia 2>/dev/null); /bin/test \\\"$current\\\" = \" & quoted form of sourcePath & \" && /bin/rm /usr/local/bin/dakia\" with administrator privileges",
                &remove_source,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        let status = terminal_command_status_for(&source);
        if !matches!(status, TerminalCommandStatus::NotSetUp) {
            return Err("The terminal command could not be verified after removal.".into());
        }
        set_terminal_menu_label(&app, &status);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Terminal setup from the app is currently available on macOS.".into())
    }
}

#[derive(Clone, Serialize)]
struct MessageContent {
    body_text: String,
    body_html: Option<String>,
    unsubscribe_kind: Option<String>,
    attachments: Vec<Attachment>,
}

/// The reader must distinguish content that cannot change on another fetch
/// from a transient provider failure. Keep the IPC payload deliberately small:
/// parser and provider diagnostics are useful locally, but are not safe UI
/// text.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MessageContentErrorKind {
    ResourceLimit,
    Malformed,
    Undecodable,
    Unsupported,
    Transient,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct MessageContentCommandError {
    kind: MessageContentErrorKind,
}

/// A bulk attachment save can have durable successes before a later file or
/// mailbox-identity check fails. Keep the outcome structured so the Reader
/// never tells someone that nothing was saved when a file is already present.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct AttachmentSaveCommandError {
    kind: AttachmentSaveErrorKind,
    saved_count: usize,
    total_count: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttachmentSaveErrorKind {
    Partial,
}

fn partial_attachment_save_error(saved_count: usize, total_count: usize) -> String {
    serde_json::to_string(&AttachmentSaveCommandError {
        kind: AttachmentSaveErrorKind::Partial,
        saved_count,
        total_count,
    })
    .expect("attachment save error is serializable")
}

impl MessageContentCommandError {
    fn from_failure(failure: &str) -> Self {
        let failure = failure.to_ascii_lowercase();
        let resource_limit = [
            "mime_raw_message_too_large",
            "mime_headers_too_large",
            "mime_too_many_parts",
            "mime_multipart_nesting_too_deep",
            "mime_resolved_html_too_large",
            "mime safety limit",
            "safety limit",
            "message has too many attachments",
            "message has more than",
            "message display body exceeds",
            "message display body size overflow",
            "attachment bytes overflowed",
            "mime part count overflow",
        ]
        .iter()
        .any(|marker| failure.contains(marker));
        let malformed = [
            "bodystructure",
            "mime part headers are malformed",
            "mime part parser omitted",
            "message parser could not find",
        ]
        .iter()
        .any(|marker| failure.contains(marker));
        let kind = if resource_limit {
            MessageContentErrorKind::ResourceLimit
        } else if failure.contains("mime_content_undecodable")
            || failure.contains("mime_invalid_part_graph")
        {
            MessageContentErrorKind::Undecodable
        } else if failure.contains("unsupported transfer encoding") {
            MessageContentErrorKind::Unsupported
        } else if malformed {
            MessageContentErrorKind::Malformed
        } else {
            MessageContentErrorKind::Transient
        };
        Self { kind }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopNotification {
    title: String,
    body: String,
    account_id: Option<String>,
    message_id: Option<String>,
    thread_id: Option<String>,
    rfc_message_id: Option<String>,
    count: usize,
    sound: Option<String>,
}

#[cfg(any(target_os = "macos", test))]
const NATIVE_NOTIFICATION_UNAVAILABLE_ERROR: &str =
    "Desktop notifications are unavailable because Dakia could not verify its application identity";

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Default)]
struct NativeNotificationAvailability(Arc<AtomicBool>);

#[cfg(any(target_os = "macos", test))]
impl NativeNotificationAvailability {
    fn mark_available(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn require_available(&self) -> Result<(), String> {
        self.0
            .load(Ordering::SeqCst)
            .then_some(())
            .ok_or_else(|| NATIVE_NOTIFICATION_UNAVAILABLE_ERROR.to_owned())
    }
}

#[cfg(any(target_os = "macos", test))]
fn initialize_native_notification_application(
    availability: &NativeNotificationAvailability,
    application: &str,
    preflight: impl FnOnce(&str) -> Result<(), String>,
    initialize: impl FnOnce(&str) -> Result<(), String>,
    report_failure: impl FnOnce(&str),
) {
    let result = preflight(application).and_then(|()| initialize(application));
    match result {
        Ok(()) => availability.mark_available(),
        Err(error) => report_failure(&error),
    }
}

#[cfg(any(target_os = "macos", test))]
fn preflight_native_notification_application(
    application: &str,
    expected_app: Option<&Path>,
    resolve: impl FnOnce(&str) -> Result<Vec<PathBuf>, String>,
) -> Result<(), String> {
    let resolved_apps = resolve(application)?;
    if resolved_apps.is_empty() {
        return Err(format!(
            "LaunchServices did not resolve application {application}"
        ));
    }
    if resolved_apps
        .iter()
        .any(|path| path.as_os_str().is_empty() || !path.is_absolute())
    {
        return Err(format!(
            "LaunchServices returned a non-absolute path for application {application}"
        ));
    }
    let Some(expected_app) = expected_app else {
        return Ok(());
    };
    let expected_app = expected_app.canonicalize().map_err(|error| {
        format!(
            "canonicalize current application bundle {}: {error}",
            expected_app.display()
        )
    })?;
    let resolved_current_app = resolved_apps.into_iter().any(|resolved_app| {
        resolved_app
            .canonicalize()
            .is_ok_and(|resolved_app| resolved_app == expected_app)
    });
    if !resolved_current_app {
        return Err(format!(
            "LaunchServices did not resolve application {application} to the current bundle {}",
            expected_app.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn current_macos_application_bundle() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve current executable: {error}"))?
        .canonicalize()
        .map_err(|error| format!("canonicalize current executable: {error}"))?;
    let macos = executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "MacOS"))
        .ok_or_else(|| {
            format!(
                "current executable is not inside an application MacOS directory: {}",
                executable.display()
            )
        })?;
    let contents = macos
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .ok_or_else(|| {
            format!(
                "current executable is not inside an application Contents directory: {}",
                executable.display()
            )
        })?;
    let app = contents
        .parent()
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
        .ok_or_else(|| {
            format!(
                "current executable is not inside an application bundle: {}",
                executable.display()
            )
        })?;
    app.canonicalize()
        .map_err(|error| format!("canonicalize current application bundle: {error}"))
}

#[cfg(target_os = "macos")]
fn resolve_launch_services_application_urls(application: &str) -> Result<Vec<PathBuf>, String> {
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const MAX_APPLICATION_URLS: isize = 4_096;
    const INITIAL_PATH_CAPACITY: usize = 4_096;
    const MAX_PATH_CAPACITY: usize = 65_536;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFGetTypeID(value: *const c_void) -> usize;
        fn CFRelease(value: *const c_void);
        fn CFStringCreateWithBytes(
            allocator: *const c_void,
            bytes: *const u8,
            length: isize,
            encoding: u32,
            is_external_representation: u8,
        ) -> *const c_void;
        fn CFURLGetFileSystemRepresentation(
            url: *const c_void,
            resolve_against_base: u8,
            buffer: *mut u8,
            max_buffer_length: isize,
        ) -> u8;
        fn CFURLGetTypeID() -> usize;
    }
    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSCopyApplicationURLsForBundleIdentifier(
            bundle_identifier: *const c_void,
            out_error: *mut *const c_void,
        ) -> *const c_void;
    }

    struct OwnedCoreFoundation(*const c_void);
    impl Drop for OwnedCoreFoundation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: This wrapper owns a create/copy-rule Core Foundation value.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    let length = isize::try_from(application.len())
        .map_err(|_| "application identifier is too long".to_owned())?;
    // SAFETY: The UTF-8 byte slice remains alive for the duration of the call, and
    // the returned create-rule object is released by OwnedCoreFoundation.
    let identifier = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            application.as_ptr(),
            length,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    if identifier.is_null() {
        return Err("Core Foundation could not represent the application identifier".to_owned());
    }
    let identifier = OwnedCoreFoundation(identifier);
    // SAFETY: identifier is a live CFStringRef. A null error pointer is permitted,
    // and the returned copy-rule array is released below.
    let applications =
        unsafe { LSCopyApplicationURLsForBundleIdentifier(identifier.0, ptr::null_mut()) };
    if applications.is_null() {
        return Ok(Vec::new());
    }
    let applications = OwnedCoreFoundation(applications);
    // SAFETY: applications is the CFArrayRef returned by LaunchServices.
    let count = unsafe { CFArrayGetCount(applications.0) };
    if !(0..=MAX_APPLICATION_URLS).contains(&count) {
        return Err(format!(
            "LaunchServices returned an invalid application URL count: {count}"
        ));
    }

    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: index is bounded by the count read from the same live CFArrayRef.
        let url = unsafe { CFArrayGetValueAtIndex(applications.0, index) };
        if url.is_null() {
            return Err(format!(
                "LaunchServices returned a null application URL at index {index}"
            ));
        }
        // SAFETY: url is a non-null Core Foundation value borrowed from applications.
        if unsafe { CFGetTypeID(url) } != unsafe { CFURLGetTypeID() } {
            return Err(format!(
                "LaunchServices returned a non-URL application value at index {index}"
            ));
        }

        let mut capacity = INITIAL_PATH_CAPACITY;
        let path = loop {
            let mut buffer = vec![0_u8; capacity];
            let max_buffer_length = isize::try_from(buffer.len())
                .map_err(|_| "LaunchServices application path buffer is too large".to_owned())?;
            // SAFETY: url is a live CFURLRef and buffer is writable for its full length.
            let success = unsafe {
                CFURLGetFileSystemRepresentation(url, 1, buffer.as_mut_ptr(), max_buffer_length)
            };
            if success != 0 {
                let end = buffer.iter().position(|byte| *byte == 0).ok_or_else(|| {
                    "LaunchServices application path was not null-terminated".to_owned()
                })?;
                break PathBuf::from(OsString::from_vec(buffer[..end].to_vec()));
            }
            if capacity == MAX_PATH_CAPACITY {
                return Err(format!(
                    "LaunchServices application path at index {index} exceeds {MAX_PATH_CAPACITY} bytes"
                ));
            }
            capacity = (capacity * 2).min(MAX_PATH_CAPACITY);
        };
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(any(target_os = "macos", test))]
fn with_native_notification_identity<T>(
    availability: &NativeNotificationAvailability,
    show: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    availability.require_available()?;
    show()
}

#[cfg(any(target_os = "macos", test))]
fn notification_has_reader_target(notification: &DesktopNotification) -> bool {
    notification.count == 1
        && notification
            .account_id
            .as_deref()
            .is_some_and(|account_id| !account_id.trim().is_empty())
        && [
            notification.message_id.as_deref(),
            notification.rfc_message_id.as_deref(),
            notification.thread_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod desktop_notification_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn single_message_notifications_preserve_reader_locators_without_focusing_main() {
        let notification = DesktopNotification {
            title: "New message".into(),
            body: "A reply arrived".into(),
            account_id: Some("account-1".into()),
            message_id: Some("account-1:INBOX:7".into()),
            thread_id: Some("root@example.test".into()),
            rfc_message_id: Some("<reply@example.test>".into()),
            count: 1,
            sound: None,
        };
        assert!(notification_has_reader_target(&notification));
        let value = serde_json::to_value(notification).unwrap();
        assert_eq!(value["threadId"], "root@example.test");
        assert_eq!(value["rfcMessageId"], "<reply@example.test>");

        let grouped = DesktopNotification {
            title: "New messages".into(),
            body: "Several messages arrived".into(),
            account_id: Some("account-1".into()),
            message_id: Some("account-1:INBOX:7".into()),
            thread_id: Some("root@example.test".into()),
            rfc_message_id: Some("<reply@example.test>".into()),
            count: 2,
            sound: None,
        };
        assert!(!notification_has_reader_target(&grouped));
    }

    #[test]
    fn notification_identity_failure_is_nonfatal_and_blocks_the_show_boundary() {
        let availability = NativeNotificationAvailability::default();
        let initialize_calls = AtomicUsize::new(0);

        let reported_error = Mutex::new(None);
        initialize_native_notification_application(
            &availability,
            "dev.dakia.fictional-provider-reader",
            |_| Err("LaunchServices did not resolve the application".to_owned()),
            |application| {
                initialize_calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(application, "dev.dakia.fictional-provider-reader");
                Ok(())
            },
            |error| *reported_error.lock().expect("reported error lock") = Some(error.to_owned()),
        );
        let error = reported_error
            .lock()
            .expect("reported error lock")
            .clone()
            .expect("identity initialization must report its failure without propagating it");
        assert!(error.contains("LaunchServices"));
        assert_eq!(initialize_calls.load(Ordering::SeqCst), 0);

        let show_calls = AtomicUsize::new(0);
        let error = with_native_notification_identity(&availability, || {
            show_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect_err("an unavailable identity must reject delivery");
        assert_eq!(error, NATIVE_NOTIFICATION_UNAVAILABLE_ERROR);
        assert_eq!(show_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn verified_notification_identity_allows_exactly_one_show_boundary_call() {
        let availability = NativeNotificationAvailability::default();
        initialize_native_notification_application(
            &availability,
            "dev.dakia.mail",
            |_| Ok(()),
            |application| {
                assert_eq!(application, "dev.dakia.mail");
                Ok(())
            },
            |_| panic!("a successful identity initialization must not report failure"),
        );

        let show_calls = AtomicUsize::new(0);
        with_native_notification_identity(&availability, || {
            show_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("verified identity permits delivery");
        assert_eq!(show_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn notification_identity_preflight_rejects_a_different_registered_bundle() {
        let root = tempfile::tempdir().unwrap();
        let current_app = root.path().join("Dakia.app");
        let other_app = root.path().join("Other Dakia.app");
        std::fs::create_dir(&current_app).unwrap();
        std::fs::create_dir(&other_app).unwrap();

        let error =
            preflight_native_notification_application("dev.dakia.mail", Some(&current_app), |_| {
                Ok(vec![other_app])
            })
            .expect_err("a matching bundle identifier at another path is insufficient");
        assert!(error.contains("current bundle"));
        assert!(error.contains(&current_app.display().to_string()));
    }

    #[test]
    fn notification_identity_preflight_runs_before_process_global_initialization() {
        let availability = NativeNotificationAvailability::default();
        let order = Mutex::new(Vec::new());
        initialize_native_notification_application(
            &availability,
            "dev.dakia.mail",
            |_| {
                order.lock().unwrap().push("preflight");
                Ok(())
            },
            |_| {
                order.lock().unwrap().push("initialize");
                Ok(())
            },
            |_| panic!("successful initialization must not report failure"),
        );
        assert_eq!(*order.lock().unwrap(), ["preflight", "initialize"]);
        availability.require_available().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_services_preflight_rejects_an_absent_identifier_without_mutation() {
        let absent = resolve_launch_services_application_urls(&format!(
            "dev.dakia.notification-preflight.absent.{}",
            Uuid::new_v4()
        ))
        .unwrap();
        assert!(absent.is_empty());
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddAccountInput {
    draft: AccountDraft,
    password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountConnection {
    account: Account,
    reused_existing_account: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAccountInput {
    id: Uuid,
    account_name: String,
    display_name: String,
    imap_host: String,
    imap_port: u16,
    imap_security: dakia_core::provider::Security,
    smtp_host: String,
    smtp_port: u16,
    smtp_security: dakia_core::provider::Security,
    archive_mailbox: String,
    spam_mailbox: String,
    password: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiInput {
    provider: String,
    base_url: Option<String>,
    model: String,
    api_key: Option<String>,
    executable: Option<PathBuf>,
    model_path: Option<PathBuf>,
    message_ids: Vec<String>,
    #[serde(default)]
    message_targets: Vec<AiMessageTarget>,
    instruction: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiMessageTarget {
    message_id: String,
    expected_uid_validity: Option<i64>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UnsubscribeResult {
    Completed {
        #[serde(rename = "cleanupTarget")]
        cleanup_target: Option<SenderCleanupTarget>,
    },
    OpenedWeb {
        #[serde(rename = "cleanupTarget")]
        cleanup_target: Option<SenderCleanupTarget>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SenderCleanupTarget {
    account_id: Uuid,
    sender_name: Option<String>,
    sender_address: String,
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Server replies are untrusted and can echo credentials or message content.
/// Preserve the delivery outcome while keeping provider text out of IPC.
fn send_command_error(failure: anyhow::Error) -> String {
    use dakia_core::SendFailureKind;
    let kind = failure
        .downcast_ref::<dakia_core::SendFailure>()
        .map(|failure| failure.kind)
        .unwrap_or(SendFailureKind::DeliveryUncertain);
    let message = match kind {
        SendFailureKind::NotSubmitted => {
            "The message was not sent. Check your account and try again."
        }
        SendFailureKind::DeliveryUncertain => {
            "The server did not confirm delivery. Check your Sent folder before sending again."
        }
        SendFailureKind::SentCopyFailed => {
            "The message was sent, but a copy could not be saved in Sent. Do not send it again."
        }
    };
    serde_json::to_string(&dakia_core::SendFailure {
        kind,
        message: message.into(),
    })
    .expect("send failure contains only serializable strings")
}

fn sender_cleanup_target(message: &MailSummary, account_id: Uuid) -> Option<SenderCleanupTarget> {
    let sender_address = normalize_sender_address(&message.from_address)?;
    let sender_name = message
        .from_name
        .as_deref()
        .map(|name| {
            name.chars()
                .filter(|character| !character.is_control())
                .take(256)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|name| !name.is_empty());
    Some(SenderCleanupTarget {
        account_id,
        sender_name,
        sender_address,
    })
}

fn unsubscribe_email(
    account_id: Uuid,
    to: String,
    subject: String,
    body: String,
) -> Result<ComposeMessage, String> {
    if to.is_empty()
        || to.len() > 320
        || to.contains(['\r', '\n', '\0'])
        || subject.len() > 998
        || subject.contains(['\r', '\n', '\0'])
        || body.len() > 64 * 1024
        || body.contains('\0')
    {
        return Err("This message has an invalid unsubscribe email request".into());
    }
    Ok(ComposeMessage {
        account_id,
        to: vec![to],
        cc: vec![],
        bcc: vec![],
        subject,
        body_text: body,
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
    })
}

#[cfg(test)]
mod unsubscribe_email_tests {
    use super::*;

    #[test]
    fn builds_a_plain_single_recipient_message() {
        let draft = unsubscribe_email(
            Uuid::nil(),
            "token@unsubscribe.example".into(),
            "unsubscribe".into(),
            "Please unsubscribe me".into(),
        )
        .expect("valid unsubscribe email");

        assert_eq!(draft.to, ["token@unsubscribe.example"]);
        assert!(draft.cc.is_empty());
        assert!(draft.bcc.is_empty());
        assert_eq!(draft.subject, "unsubscribe");
        assert_eq!(draft.body_text, "Please unsubscribe me");
        assert!(draft.body_html.is_none());
        assert!(draft.attachments.is_empty());
    }

    #[test]
    fn rejects_recipient_and_subject_header_injection() {
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com\r\nBcc: victim@example.com".into(),
            String::new(),
            String::new(),
        )
        .is_err());
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com".into(),
            "unsubscribe\r\nBcc: victim@example.com".into(),
            String::new(),
        )
        .is_err());
    }

    #[test]
    fn rejects_oversized_mailto_content() {
        assert!(unsubscribe_email(
            Uuid::nil(),
            "token@example.com".into(),
            String::new(),
            "x".repeat(64 * 1024 + 1),
        )
        .is_err());
    }

    #[test]
    fn unsubscribe_result_exposes_an_immutable_sender_cleanup_target() {
        let result = UnsubscribeResult::OpenedWeb {
            cleanup_target: Some(SenderCleanupTarget {
                account_id: Uuid::nil(),
                sender_name: Some("Newsletter".into()),
                sender_address: "news@example.test".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "kind": "opened_web",
                "cleanupTarget": {
                    "accountId": Uuid::nil(),
                    "senderName": "Newsletter",
                    "senderAddress": "news@example.test"
                }
            })
        );
    }

    #[test]
    fn malformed_sender_does_not_block_the_unsubscribe_result() {
        assert!(normalize_sender_address("not a mailbox").is_none());
        assert_eq!(
            serde_json::to_value(UnsubscribeResult::Completed {
                cleanup_target: None,
            })
            .unwrap(),
            serde_json::json!({ "kind": "completed", "cleanupTarget": null })
        );
    }
}

#[tauri::command]
async fn message_attachments(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<Vec<Attachment>, String> {
    Ok(
        load_message_content(state.inner(), &message_id, expected_uid_validity)
            .await?
            .attachments,
    )
}

#[tauri::command]
async fn message_content(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<MessageContent, MessageContentCommandError> {
    load_message_content(state.inner(), &message_id, expected_uid_validity)
        .await
        .map_err(|failure| MessageContentCommandError::from_failure(&failure))
}

fn message_identity_changed_error() -> String {
    "This message changed. Refresh the mailbox before trying again.".to_owned()
}

fn same_message_epoch_identity(target: &MailSummary, current: &MailSummary) -> bool {
    target.id == current.id
        && target.account_id == current.account_id
        && target.mailbox == current.mailbox
        && target.uid == current.uid
        && target.mailbox_uid_validity.is_some()
        && target.mailbox_uid_validity == current.mailbox_uid_validity
}

#[cfg(test)]
fn same_export_identity(before: &MailSummary, after: &MailSummary) -> bool {
    before.account_id == after.account_id
        && before.mailbox == after.mailbox
        && before.uid == after.uid
        && before.message_id == after.message_id
        && before.received_at == after.received_at
        && before.mailbox_uid_validity == after.mailbox_uid_validity
}

async fn current_message_at_epoch(
    store: &Store,
    message_id: &str,
    expected_uid_validity: Option<i64>,
) -> Result<MailSummary, String> {
    let message = store
        .message(message_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message not found".to_owned())?;
    let expected = expected_uid_validity
        .filter(|value| *value > 0)
        .ok_or_else(message_identity_changed_error)?;
    if message.mailbox_uid_validity != Some(expected) {
        return Err(message_identity_changed_error());
    }
    Ok(message)
}

async fn confirm_message_epoch(store: &Store, target: &MailSummary) -> Result<MailSummary, String> {
    let current = store
        .message(&target.id)
        .await
        .map_err(error)?
        .ok_or_else(message_identity_changed_error)?;
    if !same_message_epoch_identity(target, &current) {
        return Err(message_identity_changed_error());
    }
    Ok(current)
}

async fn cached_message_content(
    store: &Store,
    target: &MailSummary,
) -> Result<Option<MessageContent>, String> {
    let message_id = &target.id;
    if let Some((body_text, body_html)) = store.starred_body(message_id).await.map_err(error)? {
        confirm_message_epoch(store, target).await?;
        let message = store
            .message(message_id)
            .await
            .map_err(error)?
            .ok_or_else(message_identity_changed_error)?;
        if !same_message_epoch_identity(target, &message) {
            return Err(message_identity_changed_error());
        }
        let attachments = store
            .starred_attachment_metadata(message_id)
            .await
            .map_err(error)?;
        confirm_message_epoch(store, target).await?;
        return Ok(Some(MessageContent {
            body_text,
            body_html,
            unsubscribe_kind: message.unsubscribe_kind,
            attachments: attachments
                .into_iter()
                .filter(is_downloadable_attachment)
                .collect(),
        }));
    }
    if let Some(cached) = store
        .cached_message_content(message_id)
        .await
        .map_err(error)?
    {
        confirm_message_epoch(store, target).await?;
        return Ok(Some(MessageContent {
            body_text: cached.body_text,
            body_html: cached.body_html,
            unsubscribe_kind: cached.unsubscribe_kind,
            attachments: cached
                .attachments
                .into_iter()
                .filter(is_downloadable_attachment)
                .collect(),
        }));
    }
    confirm_message_epoch(store, target).await?;
    Ok(None)
}

fn content_from_message(message: MailSummary) -> MessageContent {
    MessageContent {
        body_text: message.body_text,
        body_html: message.body_html,
        unsubscribe_kind: message.unsubscribe_kind,
        attachments: message
            .attachments
            .into_iter()
            .map(|item| item.attachment)
            .filter(is_downloadable_attachment)
            .collect(),
    }
}

async fn load_message_content(
    state: &Arc<AppState>,
    message_id: &str,
    expected_uid_validity: Option<i64>,
) -> Result<MessageContent, String> {
    let target = current_message_at_epoch(&state.store, message_id, expected_uid_validity).await?;
    if let Some(cached) = cached_message_content(&state.store, &target).await? {
        if !looks_like_misclassified_text_body(&cached) {
            return Ok(cached);
        }
    }

    let mut waited = Duration::ZERO;
    let claim = loop {
        confirm_message_epoch(&state.store, &target).await?;
        match state
            .store
            .acquire_message_content_fetch_outcome(message_id)
            .await
            .map_err(error)?
        {
            MessageContentFetchAcquire::Claimed(claim) => break claim,
            MessageContentFetchAcquire::Missing => return Err("Message not found".to_owned()),
            MessageContentFetchAcquire::Busy => {}
        }
        if let Some(cached) = cached_message_content(&state.store, &target).await? {
            if !looks_like_misclassified_text_body(&cached) {
                return Ok(cached);
            }
        }
        if waited >= Duration::from_secs(60) {
            return Err("Timed out waiting for message content".to_owned());
        }
        let delay = Duration::from_millis(50);
        tokio::time::sleep(delay).await;
        waited += delay;
    };

    let result = async {
        confirm_message_epoch(&state.store, &target).await?;
        let cached_before_fetch = cached_message_content(&state.store, &target).await?;
        if let Some(cached) = &cached_before_fetch {
            if !looks_like_misclassified_text_body(cached) {
                return Ok(cached.clone());
            }
        }
        let account_id = Uuid::parse_str(&target.account_id).map_err(error)?;
        let account = state
            .store
            .account(account_id)
            .await
            .map_err(error)?
            .ok_or_else(|| "Account not found".to_owned())?;
        confirm_message_epoch(&state.store, &target).await?;
        let fetched = MailService::new(state.store.clone())
            .fetch_message(&account, &target.mailbox, target.uid as u32)
            .await
            .map_err(error)?;
        if !same_message_epoch_identity(&target, &fetched) {
            return Err(message_identity_changed_error());
        }
        if !state
            .store
            .publish_message_content_at_epoch(&target, &fetched)
            .await
            .map_err(error)?
        {
            return Err(message_identity_changed_error());
        }
        let canonical = confirm_message_epoch(&state.store, &target).await?;
        Ok(content_from_message(canonical))
    }
    .await;
    if let Err(release_error) = claim.release().await {
        tracing::warn!(%release_error, %message_id, "could not release message-content fetch claim");
    }
    result
}

fn looks_like_misclassified_text_body(content: &MessageContent) -> bool {
    content.body_text.trim().is_empty()
        && content
            .body_html
            .as_deref()
            .is_none_or(|html| html.trim().is_empty())
        && content.attachments.iter().any(|attachment| {
            attachment.is_inline
                && attachment.filename == "attachment"
                && matches!(attachment.mime_type.as_str(), "text/plain" | "text/html")
        })
}

#[cfg(test)]
mod message_content_repair_tests {
    use super::*;
    use dakia_core::AttachmentPresentation;

    struct UnexpectedClassifier;

    impl EmailClassifier for UnexpectedClassifier {
        fn classify(
            &mut self,
            _emails: &[EmailClassificationInput],
        ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
            panic!("message-content loading must not invoke email classification")
        }
    }

    fn message_content_test_state(store: Store) -> Arc<AppState> {
        Arc::new(AppState {
            realtime: RealtimeSyncManager::new(store.clone()),
            store,
            data_dir: PathBuf::new(),
            classifier: Mutex::new(Box::new(UnexpectedClassifier)),
            classification_owner: "message-content-test".into(),
            classification: Arc::new(ClassificationScheduler::default()),
            mail_rebuilds: Mutex::new(HashMap::new()),
            mail_rebuild_running: Mutex::new(HashSet::new()),
            mail_rebuild_cancellations: MailRebuildCancellations::default(),
            account_operations: AccountOperationLocks::default(),
            remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
            translation_downloads: Mutex::new(HashMap::new()),
        })
    }

    fn attachment(filename: &str, mime_type: &str, is_inline: bool) -> Attachment {
        Attachment {
            id: "message-1:0".into(),
            message_id: "message-1".into(),
            filename: filename.into(),
            mime_type: mime_type.into(),
            size_bytes: 42,
            size_kind: dakia_core::AttachmentSizeKind::Decoded,
            is_inline,
            presentation: AttachmentPresentation::Downloadable,
            is_potentially_unsafe: false,
        }
    }

    #[test]
    fn refetches_empty_content_with_phantom_inline_text_attachments() {
        let content = MessageContent {
            body_text: String::new(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: vec![
                attachment("attachment", "text/plain", true),
                attachment("attachment", "text/html", true),
            ],
        };

        assert!(looks_like_misclassified_text_body(&content));
    }

    #[test]
    fn preserves_legitimate_empty_messages_and_named_text_attachments() {
        let empty = MessageContent {
            body_text: String::new(),
            body_html: None,
            unsubscribe_kind: None,
            attachments: Vec::new(),
        };
        let named_attachment = MessageContent {
            attachments: vec![attachment("notes.txt", "text/plain", true)],
            ..empty
        };

        assert!(!looks_like_misclassified_text_body(&named_attachment));
    }

    #[tokio::test]
    async fn deleted_message_content_fails_without_waiting_for_a_fetch_claim() {
        let state = message_content_test_state(Store::in_memory().await.expect("test store"));

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            load_message_content(&state, "deleted-message", Some(1)),
        )
        .await
        .expect("a deleted message must not wait for the claim timeout");

        assert!(matches!(result, Err(error) if error == "Message not found"));
    }

    #[test]
    fn message_content_error_envelope_classifies_mime_failures_without_details() {
        let cases = [
            ("mime_raw_message_too_large", "resource_limit"),
            ("mime_content_undecodable", "undecodable"),
            ("mime_invalid_part_graph", "undecodable"),
            (
                "MIME part uses an unsupported transfer encoding",
                "unsupported",
            ),
            ("BODYSTRUCTURE part is not a list", "malformed"),
            (
                "message display body exceeds the 50 MiB safety limit",
                "resource_limit",
            ),
            (
                "message display part exceeds the 25 MiB safety limit",
                "resource_limit",
            ),
            (
                "message parser could not find RFC 5322 headers",
                "malformed",
            ),
            ("IMAP connection reset by peer", "transient"),
        ];

        for (failure, kind) in cases {
            let serialized =
                serde_json::to_value(MessageContentCommandError::from_failure(failure))
                    .expect("message-content error must serialize for Tauri IPC");
            assert_eq!(serialized, serde_json::json!({ "kind": kind }));
            assert!(!serialized.to_string().contains(failure));
        }
    }
}

#[tauri::command]
async fn save_attachment<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    attachment_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<String, String> {
    let (target, account, epoch) =
        remote_message_at_epoch(state.inner(), &message_id, expected_uid_validity).await?;
    let attachment = MailService::new(state.store.clone())
        .fetch_attachment_at_epoch(
            &account,
            &target.mailbox,
            target.uid as u32,
            &attachment_id,
            epoch,
        )
        .await
        .map_err(error)?;
    confirm_message_epoch(&state.store, &target).await?;
    save_to_downloads(&app, &attachment.attachment, &attachment.bytes).map_err(error)
}

#[tauri::command]
async fn export_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<String, String> {
    let (target, account, epoch) =
        remote_message_at_epoch(state.inner(), &message_id, expected_uid_validity).await?;
    let account_id = account.id;
    let _operation = state.account_operations.acquire(account_id).await;
    confirm_message_epoch(&state.store, &target).await?;
    let bytes = MailService::new(state.store.clone())
        .fetch_raw_message_at_epoch(&account, &target.mailbox, target.uid as u32, epoch)
        .await
        .map_err(error)?;
    confirm_message_epoch(&state.store, &target).await?;
    save_eml_to_downloads(&app, &target.subject, &bytes).map_err(error)
}

#[tauri::command]
async fn save_all_attachments<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<Vec<String>, String> {
    let (target, _account, _epoch) =
        remote_message_at_epoch(state.inner(), &message_id, expected_uid_validity).await?;
    let attachments = fetch_full_remote_message_at_epoch(state.inner(), &target)
        .await?
        .attachments
        .into_iter()
        .filter(|item| is_downloadable_attachment(&item.attachment))
        .collect::<Vec<_>>();
    confirm_message_epoch(&state.store, &target).await?;
    let total_count = attachments.len();
    let mut saved = Vec::with_capacity(attachments.len());
    #[cfg(feature = "native-mail-safety-fixture")]
    let mut fixture_partial_after_first_save = false;
    for attachment in attachments {
        #[cfg(feature = "native-mail-safety-fixture")]
        let confirmation = if fixture_partial_after_first_save {
            Err("fixture-only partial-save fault after one successful file".into())
        } else {
            confirm_message_epoch(&state.store, &target)
                .await
                .map(|_| ())
        };
        #[cfg(not(feature = "native-mail-safety-fixture"))]
        let confirmation = confirm_message_epoch(&state.store, &target)
            .await
            .map(|_| ());
        save_attachment_batch_step(&mut saved, total_count, confirmation, || {
            save_to_downloads(&app, &attachment.attachment, &attachment.bytes).map_err(error)
        })?;
        #[cfg(all(test, feature = "native-mail-safety-fixture"))]
        attachment_command_artifact_tests::after_successful_bulk_save_for_test(
            &state.store,
            &target,
            saved.len(),
        )
        .await;
        // This hook is compiled only into the fictional native fixture build.
        // It consumes an owned one-shot marker after the first real publish;
        // the next iteration takes the normal partial-error branch before it
        // can write another file.
        #[cfg(feature = "native-mail-safety-fixture")]
        {
            fixture_partial_after_first_save =
                attachment_partial_fault::inject_partial_after_successful_bulk_save(saved.len())
                    .map_err(|_| partial_attachment_save_error(saved.len(), total_count))?;
        }
    }
    Ok(saved)
}

fn save_attachment_batch_step<F>(
    saved: &mut Vec<String>,
    total_count: usize,
    confirmation: Result<(), String>,
    save: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<String, String>,
{
    if confirmation.is_err() {
        return Err(partial_attachment_save_error(saved.len(), total_count));
    }
    match save() {
        Ok(path) => {
            saved.push(path);
            Ok(())
        }
        Err(_) => Err(partial_attachment_save_error(saved.len(), total_count)),
    }
}

#[tauri::command]
async fn forward_attachments(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<Vec<DroppedAttachment>, String> {
    let (target, _account, _epoch) =
        remote_message_at_epoch(state.inner(), &message_id, expected_uid_validity).await?;
    let attachments = fetch_full_remote_message_at_epoch(state.inner(), &target)
        .await?
        .attachments
        .into_iter()
        .filter(|item| is_downloadable_attachment(&item.attachment))
        .collect::<Vec<_>>();
    confirm_message_epoch(&state.store, &target).await?;
    if attachments.len() > MAX_DROPPED_ATTACHMENTS {
        return Err(format!(
            "This message has more than {MAX_DROPPED_ATTACHMENTS} attachments"
        ));
    }
    let total_bytes = attachments
        .iter()
        .map(|attachment| attachment.bytes.len() as u64)
        .sum::<u64>();
    if attachments
        .iter()
        .any(|attachment| attachment.bytes.len() as u64 > MAX_DROPPED_ATTACHMENT_BYTES)
        || total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES
    {
        return Err("The original attachments exceed the forwarding limit".into());
    }
    Ok(attachments
        .into_iter()
        .map(|attachment| DroppedAttachment {
            filename: attachment.attachment.filename,
            mime_type: attachment.attachment.mime_type,
            size_bytes: attachment.bytes.len() as u64,
            content_base64: STANDARD.encode(attachment.bytes),
        })
        .collect())
}

fn is_downloadable_attachment(attachment: &Attachment) -> bool {
    attachment.presentation.is_downloadable()
}

/// Authoritative foreground parsing corrects paperclip state for every
/// message. Starred content is written only when the current local row remains
/// starred; a fetched provider snapshot must never undo a concurrent unstar.
#[cfg(test)]
async fn persist_foreground_message_content(
    store: &Store,
    message: &MailSummary,
    content: &CachedMessageContent,
) -> Result<bool, String> {
    let exists = store
        .update_message_attachment_state(&message.id, message.has_attachments)
        .await
        .map_err(error)?;
    if !exists {
        return Ok(false);
    }
    if !message.is_flagged {
        return Ok(false);
    }
    store
        .cache_starred_message_content(&message.id, content.clone())
        .await
        .map_err(error)
}

#[cfg(test)]
mod attachment_presentation_command_tests {
    use super::*;
    use chrono::Utc;
    use dakia_core::{storage::AttachmentData, AttachmentPresentation, AttachmentSizeKind};
    use tempfile::tempdir;

    fn attachment(id: &str, presentation: AttachmentPresentation) -> Attachment {
        Attachment {
            id: id.into(),
            message_id: "message-attachment-presentation".into(),
            filename: format!("{id}.bin"),
            mime_type: "application/octet-stream".into(),
            size_bytes: 3,
            size_kind: AttachmentSizeKind::Decoded,
            is_inline: matches!(presentation, AttachmentPresentation::Embedded),
            presentation,
            is_potentially_unsafe: false,
        }
    }

    fn complete_message(account_id: String, is_flagged: bool) -> MailSummary {
        MailSummary {
            mailbox_uid_validity: None,
            id: "message-attachment-presentation".into(),
            account_id,
            mailbox: "INBOX".into(),
            uid: 1,
            message_id: Some("<attachment-presentation@example.test>".into()),
            gmail_message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: "message-attachment-presentation".into(),
            subject: "Attachment presentation".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "recipient@example.test".into(),
            cc_addresses: String::new(),
            bcc_addresses: String::new(),
            reply_to_addresses: String::new(),
            received_at: Utc::now(),
            snippet: "authoritative body".into(),
            body_text: "authoritative body".into(),
            body_html: Some("<p>authoritative body</p>".into()),
            content_state: "complete".into(),
            unsubscribe_kind: None,
            unsubscribe_url: None,
            is_read: true,
            is_flagged,
            has_attachments: true,
            category: None,
            classification_confidence: None,
            classification_source: None,
            classification_signals: String::new(),
            attachments: vec![
                AttachmentData {
                    attachment: attachment("signature-logo", AttachmentPresentation::Embedded),
                    bytes: b"logo".to_vec(),
                },
                AttachmentData {
                    attachment: attachment("claim", AttachmentPresentation::Downloadable),
                    bytes: b"pdf".to_vec(),
                },
            ],
        }
    }

    fn cached_content(message: &MailSummary) -> CachedMessageContent {
        CachedMessageContent {
            body_text: message.body_text.clone(),
            body_html: message.body_html.clone(),
            unsubscribe_kind: message.unsubscribe_kind.clone(),
            attachments: message
                .attachments
                .iter()
                .filter(|item| is_downloadable_attachment(&item.attachment))
                .map(|item| item.attachment.clone())
                .collect(),
        }
    }

    async fn save_test_account(store: &Store) -> Account {
        let account = AccountDraft {
            email: "attachment-presentation@example.test".into(),
            display_name: "Attachment presentation".into(),
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
        .into_account(provider::by_id("fastmail").expect("Fastmail preset"));
        store.save_account(&account).await.expect("save account");
        account
    }

    #[test]
    fn command_boundary_exposes_only_downloadable_presentations() {
        let visible = [
            attachment("legacy", AttachmentPresentation::Unknown),
            attachment("signature", AttachmentPresentation::Embedded),
            attachment("document", AttachmentPresentation::Downloadable),
            attachment("explicit-cid", AttachmentPresentation::Both),
        ]
        .into_iter()
        .filter(is_downloadable_attachment)
        .map(|attachment| attachment.id)
        .collect::<Vec<_>>();

        assert_eq!(visible, ["document", "explicit-cid"]);
    }

    #[tokio::test]
    async fn authoritative_flagged_foreground_fetch_restores_starred_body_and_real_metadata() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = save_test_account(&store).await;

        // This is the post-migration state: the flagged catalogue message
        // survives, but its old starred body/attachment metadata was cleared.
        let catalogue = complete_message(account.id.to_string(), false);
        let message_id = catalogue.id.clone();
        store
            .upsert_messages(&[catalogue])
            .await
            .expect("save catalogue message");
        store
            .set_message_flagged(&message_id, true)
            .await
            .expect("flag message without a durable body");
        assert!(store
            .starred_body(&message_id)
            .await
            .expect("read starred body")
            .is_none());

        let fetched = complete_message(account.id.to_string(), true);
        assert!(
            persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist authoritative foreground fetch")
        );

        assert_eq!(
            store
                .starred_body(&message_id)
                .await
                .expect("read restored starred body")
                .expect("restored body")
                .0,
            "authoritative body"
        );
        let metadata = store
            .starred_attachment_metadata(&message_id)
            .await
            .expect("read restored attachment metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].id, "claim");
        assert_eq!(
            metadata[0].presentation,
            AttachmentPresentation::Downloadable
        );
    }

    #[tokio::test]
    async fn ordinary_foreground_fetch_persists_named_inline_paperclip_across_restart() {
        let directory = tempdir().expect("temporary store directory");
        let database = directory.path().join("dakia.db");
        let store = Store::open(&database).await.expect("open store");
        let account = save_test_account(&store).await;
        let mut catalogue = complete_message(account.id.to_string(), false);
        let message_id = catalogue.id.clone();
        // Header-only catalogue parsing cannot know whether an inline named
        // resource is used by the selected HTML branch.
        catalogue.has_attachments = false;
        catalogue.attachments.clear();
        store
            .upsert_messages(&[catalogue])
            .await
            .expect("save header-only catalogue message");

        let mut fetched = complete_message(account.id.to_string(), false);
        fetched.attachments[1].attachment.is_inline = true;
        assert!(
            !persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist ordinary foreground metadata")
        );
        assert!(
            store
                .message(&message_id)
                .await
                .expect("read collapsed message")
                .expect("message remains")
                .has_attachments
        );

        drop(store);
        let reopened = Store::open(&database).await.expect("reopen store");
        assert!(
            reopened
                .message(&message_id)
                .await
                .expect("read restarted message")
                .expect("message survives restart")
                .has_attachments
        );
    }

    #[tokio::test]
    async fn foreground_fetch_cannot_restore_a_message_unstarred_while_it_was_in_flight() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account = save_test_account(&store).await;
        let fetched = complete_message(account.id.to_string(), true);
        let message_id = fetched.id.clone();
        store
            .upsert_messages(std::slice::from_ref(&fetched))
            .await
            .expect("save initially starred catalogue message");

        // The remote fetch observed `\\Flagged`, then the user unstarred the
        // current local row before its response could be persisted.
        store
            .set_message_flagged(&message_id, false)
            .await
            .expect("unstar during fetch");
        assert!(
            !persist_foreground_message_content(&store, &fetched, &cached_content(&fetched))
                .await
                .expect("persist stale fetch without resurrecting the star")
        );

        assert!(
            !store
                .message(&message_id)
                .await
                .expect("read current message")
                .expect("message remains")
                .is_flagged
        );
        assert!(store
            .starred_body(&message_id)
            .await
            .expect("read starred cache")
            .is_none());
        assert!(store
            .starred_attachment_metadata(&message_id)
            .await
            .expect("read starred metadata")
            .is_empty());
    }
}

async fn fetch_full_remote_message_at_epoch(
    state: &Arc<AppState>,
    target: &MailSummary,
) -> Result<MailSummary, String> {
    let account_id = Uuid::parse_str(&target.account_id).map_err(error)?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    confirm_message_epoch(&state.store, target).await?;
    let fetched = MailService::new(state.store.clone())
        .fetch_full_message(&account, &target.mailbox, target.uid as u32)
        .await
        .map_err(error)?;
    if !same_message_epoch_identity(target, &fetched) {
        return Err(message_identity_changed_error());
    }
    confirm_message_epoch(&state.store, target).await?;
    Ok(fetched)
}

async fn remote_message_at_epoch(
    state: &Arc<AppState>,
    message_id: &str,
    expected_uid_validity: Option<i64>,
) -> Result<(MailSummary, Account, i64), String> {
    let target = current_message_at_epoch(&state.store, message_id, expected_uid_validity).await?;
    let epoch = target
        .mailbox_uid_validity
        .ok_or_else(message_identity_changed_error)?;
    let account_id = Uuid::parse_str(&target.account_id).map_err(error)?;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    confirm_message_epoch(&state.store, &target).await?;
    Ok((target, account, epoch))
}

struct OpenDroppedFile {
    filename: String,
    file: std::fs::File,
    metadata_len: u64,
}

fn open_dropped_file(path: PathBuf, index: usize) -> Result<OpenDroppedFile, String> {
    let filename = dakia_core::mail::safe_attachment_filename(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment"),
        index,
    );
    let link_metadata = std::fs::symlink_metadata(&path).map_err(error)?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err("Only regular files can be attached".into());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(&path).map_err(|open_error| {
        #[cfg(unix)]
        if open_error.raw_os_error() == Some(libc::ELOOP) {
            "Only regular files can be attached".to_owned()
        } else {
            error(open_error)
        }
        #[cfg(not(unix))]
        {
            error(open_error)
        }
    })?;
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() {
        return Err("Only regular files can be attached".into());
    }
    Ok(OpenDroppedFile {
        filename,
        file,
        metadata_len: metadata.len(),
    })
}

fn open_dropped_files(paths: Vec<PathBuf>) -> Result<Vec<OpenDroppedFile>, String> {
    if paths.len() > MAX_DROPPED_ATTACHMENTS {
        return Err(format!(
            "A message can include at most {MAX_DROPPED_ATTACHMENTS} attachments"
        ));
    }

    let mut total_bytes = 0_u64;
    let mut files = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        let file = open_dropped_file(path, index)?;
        if file.metadata_len > MAX_DROPPED_ATTACHMENT_BYTES {
            return Err(format!(
                "{} exceeds the {} MiB attachment limit",
                file.filename,
                MAX_DROPPED_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        total_bytes += file.metadata_len;
        if total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES {
            return Err(format!(
                "Attachments exceed the {} MiB total limit",
                MAX_DROPPED_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
            ));
        }
        files.push(file);
    }
    Ok(files)
}

fn materialize_dropped_files(
    files: Vec<OpenDroppedFile>,
) -> Result<Vec<DroppedAttachment>, String> {
    let mut total_bytes = 0;
    let mut attachments = Vec::with_capacity(files.len());
    for mut dropped_file in files {
        let mut bytes = Vec::with_capacity(dropped_file.metadata_len as usize);
        Read::by_ref(&mut dropped_file.file)
            .take(MAX_DROPPED_ATTACHMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() as u64 > MAX_DROPPED_ATTACHMENT_BYTES {
            return Err(format!(
                "{} exceeds the {} MiB attachment limit",
                dropped_file.filename,
                MAX_DROPPED_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        total_bytes += bytes.len() as u64;
        if total_bytes > MAX_DROPPED_ATTACHMENT_TOTAL_BYTES {
            return Err(format!(
                "Attachments exceed the {} MiB total limit",
                MAX_DROPPED_ATTACHMENT_TOTAL_BYTES / 1024 / 1024
            ));
        }
        let size_bytes = bytes.len() as u64;
        attachments.push(DroppedAttachment {
            mime_type: mime_type_for_filename(&dropped_file.filename).into(),
            filename: dropped_file.filename,
            content_base64: STANDARD.encode(bytes),
            size_bytes,
        });
    }
    Ok(attachments)
}

#[tauri::command]
async fn read_dropped_files(
    window: tauri::WebviewWindow,
    receipts: State<'_, Arc<DroppedFileReceiptStore>>,
    receipt: String,
) -> Result<Vec<DroppedAttachment>, String> {
    let files = receipts.consume(&receipt, window.label())?;
    tokio::task::spawn_blocking(move || materialize_dropped_files(files))
        .await
        .map_err(error)?
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

#[cfg(test)]
mod dropped_file_receipt_tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture_path(directory: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).expect("dropped-file fixture");
        path
    }

    fn issue(
        store: &DroppedFileReceiptStore,
        window_label: &str,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> String {
        store
            .issue_at(window_label, paths, now)
            .expect("native drop receipt")
    }

    #[test]
    fn rejects_forged_receipts_and_raw_paths() {
        let store = DroppedFileReceiptStore::default();
        assert!(store.consume("forged", "compose-1").is_err());
        assert!(store
            .consume("/Users/example/private-file", "compose-1")
            .is_err());
    }

    #[test]
    fn consumes_a_receipt_only_once() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let path = fixture_path(&directory, "drop.txt", b"original");
        let receipt = issue(&store, "compose-1", vec![path.clone()], now);

        let files = store
            .consume_at(&receipt, "compose-1", now)
            .expect("first redemption");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "drop.txt");
        assert!(store.consume_at(&receipt, "compose-1", now).is_err());
    }

    #[test]
    fn rejects_expired_receipts() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let receipt = issue(
            &store,
            "compose-1",
            vec![fixture_path(&directory, "drop.txt", b"original")],
            now,
        );

        assert!(store
            .consume_at(&receipt, "compose-1", now + DROPPED_FILE_RECEIPT_TTL)
            .is_err());
    }

    #[test]
    fn passive_expiry_drops_an_unredeemed_handle() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let receipt = issue(
            &store,
            "compose-1",
            vec![fixture_path(&directory, "drop.txt", b"original")],
            now,
        );

        store.expire_at(&receipt, now + DROPPED_FILE_RECEIPT_TTL);

        assert!(store.consume_at(&receipt, "compose-1", now).is_err());
        assert!(store.entries.lock().expect("receipt store").is_empty());
    }

    #[test]
    fn binds_receipts_to_the_originating_window_session() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let now = Instant::now();
        let path = fixture_path(&directory, "drop.txt", b"original");
        let receipt = issue(&store, "compose-origin", vec![path.clone()], now);

        assert!(store.consume_at(&receipt, "compose-attacker", now).is_err());
        let files = store
            .consume_at(&receipt, "compose-origin", now)
            .expect("originating window can still redeem");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "drop.txt");
    }

    #[test]
    fn revokes_receipts_when_the_window_session_is_destroyed() {
        let store = DroppedFileReceiptStore::default();
        let directory = tempdir().expect("tempdir");
        let receipt = store
            .issue(
                "compose-closed",
                vec![fixture_path(&directory, "drop.txt", b"original")],
            )
            .expect("receipt");

        store.revoke_window("compose-closed");

        assert!(store.consume(&receipt, "compose-closed").is_err());
    }

    #[test]
    fn rejects_more_than_the_attachment_count_limit_at_issuance() {
        let store = DroppedFileReceiptStore::default();
        let paths = (0..=MAX_DROPPED_ATTACHMENTS)
            .map(|index| PathBuf::from(format!("/native/{index}")))
            .collect();

        assert!(store.issue("compose-1", paths).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let target = directory.path().join("private.txt");
        let link = directory.path().join("dropped.txt");
        std::fs::write(&target, b"secret").expect("target");
        symlink(&target, &link).expect("symlink");

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", vec![link])
            .expect_err("symlink rejected at native drop");

        assert!(error.contains("regular files"));
    }

    #[test]
    fn rejects_total_size_before_reading_file_contents() {
        let directory = tempdir().expect("tempdir");
        let each_size = 18 * 1024 * 1024;
        let paths = (0..3)
            .map(|index| {
                let path = directory.path().join(format!("{index}.bin"));
                std::fs::File::create(&path)
                    .expect("sparse file")
                    .set_len(each_size)
                    .expect("sparse size");
                path
            })
            .collect();

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", paths)
            .expect_err("aggregate attachment size rejected at native drop");

        assert!(error.contains("total limit"));
    }

    #[test]
    fn rejects_a_file_over_the_per_attachment_limit() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("oversized.bin");
        std::fs::File::create(&path)
            .expect("sparse file")
            .set_len(MAX_DROPPED_ATTACHMENT_BYTES + 1)
            .expect("sparse size");

        let error = DroppedFileReceiptStore::default()
            .issue("compose-1", vec![path])
            .expect_err("per-file size limit rejected at native drop");

        assert!(error.contains("attachment limit"));
    }

    #[test]
    fn reads_the_opened_regular_file_handle() {
        let directory = tempdir().expect("tempdir");
        let path = fixture_path(&directory, "notes.txt", b"hello");
        let store = DroppedFileReceiptStore::default();
        let receipt = store
            .issue("compose-1", vec![path])
            .expect("native drop receipt");
        let files = store
            .consume(&receipt, "compose-1")
            .expect("receipt redemption");

        let attachments = materialize_dropped_files(files).expect("attachment");

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "notes.txt");
        assert_eq!(attachments[0].mime_type, "text/plain");
        assert_eq!(attachments[0].size_bytes, 5);
        assert_eq!(
            STANDARD
                .decode(&attachments[0].content_base64)
                .expect("base64"),
            b"hello"
        );
    }

    #[test]
    fn path_replacement_after_issuance_cannot_change_the_opened_file() {
        let directory = tempdir().expect("tempdir");
        let path = fixture_path(&directory, "drop.txt", b"original bytes");
        let moved_original = directory.path().join("moved-original.txt");
        let store = DroppedFileReceiptStore::default();
        let receipt = store
            .issue("compose-1", vec![path.clone()])
            .expect("native drop receipt");

        std::fs::rename(&path, moved_original).expect("move original inode");
        std::fs::write(&path, b"replacement secret").expect("replace dropped path");

        let files = store
            .consume(&receipt, "compose-1")
            .expect("receipt redemption");
        let attachments = materialize_dropped_files(files).expect("attachment");

        assert_eq!(
            STANDARD
                .decode(&attachments[0].content_base64)
                .expect("base64"),
            b"original bytes"
        );
    }
}

fn save_to_downloads<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    attachment: &Attachment,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let downloads = attachment_download_directory(app)?;
    let filename = dakia_core::mail::safe_attachment_filename(&attachment.filename, 0);
    Ok(save_private_download(&downloads, &filename, bytes)?
        .to_string_lossy()
        .into_owned())
}

fn attachment_download_directory<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> anyhow::Result<PathBuf> {
    #[cfg(feature = "native-mail-safety-fixture")]
    if let Some(downloads) = native_mail_safety_download_directory()? {
        return Ok(downloads);
    }
    Ok(app.path().download_dir()?)
}

/// Recheck the owned fixture directory on every save. The runner validates it
/// before startup too, but a later symlink replacement must not turn an email
/// download into a write outside the fictional acceptance run.
#[cfg(feature = "native-mail-safety-fixture")]
fn native_mail_safety_download_directory() -> anyhow::Result<Option<PathBuf>> {
    match std::env::var("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE") {
        Ok(value) if value == "1" => {}
        Ok(_) => anyhow::bail!("native mail-safety fixture marker must equal 1"),
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let root = PathBuf::from(
        std::env::var("DAKIA_NATIVE_MAIL_SAFETY_ROOT")
            .map_err(|_| anyhow::anyhow!("native mail-safety fixture root is required"))?,
    );
    let downloads = PathBuf::from(
        std::env::var("DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR").map_err(|_| {
            anyhow::anyhow!("native mail-safety fixture Downloads directory is required")
        })?,
    );
    Ok(Some(verified_native_mail_safety_download_directory(
        &root, &downloads,
    )?))
}

#[cfg(feature = "native-mail-safety-fixture")]
fn verified_native_mail_safety_download_directory(
    root: &Path,
    downloads: &Path,
) -> anyhow::Result<PathBuf> {
    if !root.is_absolute() || !downloads.is_absolute() {
        anyhow::bail!("native mail-safety fixture paths must be absolute");
    }
    let root = root.canonicalize()?;
    if std::fs::read_to_string(root.join(".dakia-native-mail-safety-root"))
        .ok()
        .as_deref()
        != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native mail-safety fixture root lacks its exact ownership sentinel");
    }
    let downloads = downloads.canonicalize()?;
    if !downloads.is_dir() || !downloads.starts_with(&root) {
        anyhow::bail!("native mail-safety fixture Downloads directory must be inside its root");
    }
    Ok(downloads)
}

fn save_eml_to_downloads(
    app: &tauri::AppHandle,
    subject: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let downloads = attachment_download_directory(app)?;
    Ok(
        save_private_download(&downloads, &eml_export_filename(subject), bytes)?
            .to_string_lossy()
            .into_owned(),
    )
}

fn eml_export_filename(subject: &str) -> String {
    let sanitized = dakia_core::mail::safe_attachment_filename(subject, 0);
    let fallback = subject
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .all(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '.' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        });
    let stem = if fallback { "message" } else { &sanitized };
    format!("{}.eml", truncate_utf8_filename_stem(stem))
}

fn truncate_utf8_filename_stem(stem: &str) -> &str {
    let maximum_stem_bytes =
        MAX_EXPORT_FILENAME_BYTES - ".eml".len() - MAX_DOWNLOAD_COLLISION_SUFFIX_BYTES;
    if stem.len() <= maximum_stem_bytes {
        return stem;
    }
    let mut end = 0;
    for (index, character) in stem.char_indices() {
        let next = index + character.len_utf8();
        if next > maximum_stem_bytes {
            break;
        }
        end = next;
    }
    &stem[..end]
}

fn save_private_download(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
) -> anyhow::Result<PathBuf> {
    save_private_download_with_writer(downloads, filename, bytes, |file, contents| {
        file.write_all(contents).and_then(|_| file.sync_all())
    })
}

/// The writer seam keeps fault tests at the exact create/write/fsync/cleanup
/// boundary. The production caller above always uses the real filesystem.
#[cfg(unix)]
fn save_private_download_with_writer<F>(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
    write_and_sync: F,
) -> anyhow::Result<PathBuf>
where
    F: FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
{
    save_private_download_with_directory_descriptor(
        downloads,
        filename,
        bytes,
        write_and_sync,
        quarantine_download,
        |_| Ok(()),
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(not(unix))]
fn save_private_download_with_writer<F>(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
    write_and_sync: F,
) -> anyhow::Result<PathBuf>
where
    F: FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
{
    save_private_download_with_operations(
        downloads,
        filename,
        bytes,
        |path| std::fs::create_dir_all(path),
        open_private_download_file,
        write_and_sync,
        quarantine_download,
    )
}

/// On Unix, pin the configured Downloads directory by descriptor before
/// creating an attachment. A configured directory symlink remains valid, but
/// later path swaps cannot redirect `openat` or cleanup to a different parent.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn save_private_download_with_directory_descriptor<W, Q, D, F, P>(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
    mut write_and_sync: W,
    mut quarantine: Q,
    mut after_directory_open: D,
    mut after_file_open: F,
    mut before_publish: P,
) -> anyhow::Result<PathBuf>
where
    W: FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    Q: FnMut(&std::fs::File) -> anyhow::Result<()>,
    D: FnMut(&std::fs::File) -> anyhow::Result<()>,
    F: FnMut() -> anyhow::Result<()>,
    P: FnMut() -> anyhow::Result<()>,
{
    let mut after_publish = || Ok(());
    save_private_download_with_directory_descriptor_and_cleanup(
        downloads,
        filename,
        bytes,
        &mut write_and_sync,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        private_download_candidate_exists_at,
        unlink_private_download_at,
        sync_private_download_directory,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn save_private_download_with_directory_descriptor_and_cleanup<W, Q, D, F, P, A, C, U, S>(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
    write_and_sync: &mut W,
    quarantine: &mut Q,
    after_directory_open: &mut D,
    after_file_open: &mut F,
    before_publish: &mut P,
    after_publish: &mut A,
    mut candidate_exists: C,
    mut unlink_temporary: U,
    mut sync_directory: S,
) -> anyhow::Result<PathBuf>
where
    W: FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    Q: FnMut(&std::fs::File) -> anyhow::Result<()>,
    D: FnMut(&std::fs::File) -> anyhow::Result<()>,
    F: FnMut() -> anyhow::Result<()>,
    P: FnMut() -> anyhow::Result<()>,
    A: FnMut() -> anyhow::Result<()>,
    C: FnMut(&std::fs::File, &std::ffi::CStr) -> std::io::Result<bool>,
    U: FnMut(&std::fs::File, &std::ffi::CStr) -> std::io::Result<()>,
    S: FnMut(&std::fs::File) -> std::io::Result<()>,
{
    if !is_safe_download_filename(filename) {
        anyhow::bail!("attachment filename is not a safe basename");
    }
    std::fs::create_dir_all(downloads)?;
    let directory = std::fs::File::open(downloads)?;
    if !directory.metadata()?.is_dir() {
        anyhow::bail!("Downloads path is not a directory");
    }
    after_directory_open(&directory)?;

    for counter in 0..10_000 {
        let final_name = CString::new(download_name(filename, counter))?;
        match candidate_exists(&directory, &final_name) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Err(error.into()),
        }
        let (temporary_name, mut file) = open_private_download_temporary_at(&directory)?;
        if let Err(hook_error) = after_file_open() {
            return Err(cleanup_temporary_download_error(
                hook_error,
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if let Err(write_error) = write_and_sync(&mut file, bytes) {
            return Err(cleanup_temporary_download_error(
                write_error.into(),
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if let Err(permission_error) = file.set_permissions(std::fs::Permissions::from_mode(0o600))
        {
            return Err(cleanup_temporary_download_error(
                permission_error.into(),
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if let Err(quarantine_error) = quarantine(&file) {
            return Err(cleanup_temporary_download_error(
                quarantine_error,
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if let Err(barrier_error) = before_publish() {
            return Err(cleanup_temporary_download_error(
                barrier_error,
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if !downloads_directory_matches(&directory, downloads)? {
            return Err(cleanup_temporary_download_error(
                anyhow::anyhow!(
                    "Downloads directory changed while saving. Retry the attachment save."
                ),
                &directory,
                &temporary_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }

        match publish_private_download_at(&directory, &temporary_name, &final_name) {
            Ok(PublishPrivateDownload::PublishedAndTemporaryRemoved) => {}
            #[cfg(not(target_os = "macos"))]
            Ok(PublishPrivateDownload::PublishedWithTemporary) => {
                if let Err(cleanup_error) = cleanup_temporary_download(
                    &directory,
                    &temporary_name,
                    &mut unlink_temporary,
                    &mut sync_directory,
                ) {
                    return Err(anyhow::anyhow!(
                        "Attachment was saved, but hidden temporary cleanup failed: {cleanup_error}"
                    ));
                }
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if let Err(cleanup_error) = cleanup_temporary_download(
                    &directory,
                    &temporary_name,
                    &mut unlink_temporary,
                    &mut sync_directory,
                ) {
                    return Err(anyhow::anyhow!(
                        "could not choose a safe filename in Downloads; hidden temporary cleanup failed: {cleanup_error}"
                    ));
                }
                continue;
            }
            Err(error) => {
                return Err(cleanup_temporary_download_error(
                    error.into(),
                    &directory,
                    &temporary_name,
                    &mut unlink_temporary,
                    &mut sync_directory,
                ));
            }
        }

        if let Err(hook_error) = after_publish() {
            return Err(cleanup_published_download_error(
                hook_error,
                &directory,
                &final_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        if let Err(_sync_error) = sync_directory(&directory) {
            tracing::warn!("attachment download directory sync failed after publication");
        }
        if !downloads_directory_matches(&directory, downloads)? {
            return Err(cleanup_published_download_error(
                anyhow::anyhow!(
                    "Downloads directory changed while publishing. Retry the attachment save."
                ),
                &directory,
                &final_name,
                &mut unlink_temporary,
                &mut sync_directory,
            ));
        }
        return Ok(downloads.join(final_name.to_string_lossy().as_ref()));
    }
    Err(anyhow::anyhow!(
        "could not choose a safe filename in Downloads"
    ))
}

/// Probe the candidate through the pinned Downloads descriptor before doing
/// the durable temporary-file work. This is advisory only: the later
/// descriptor-relative exclusive publish remains the authority for races.
/// Existing symlinks count as occupied without being followed.
#[cfg(unix)]
fn private_download_candidate_exists_at(
    directory: &std::fs::File,
    name: &std::ffi::CStr,
) -> std::io::Result<bool> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(directory),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn open_private_download_temporary_at(
    directory: &std::fs::File,
) -> anyhow::Result<(CString, std::fs::File)> {
    for _ in 0..10_000 {
        let temporary_name = CString::new(format!(".dakia-{}.partial", Uuid::new_v4()))?;
        match open_private_download_at(directory, &temporary_name) {
            Ok(file) => return Ok((temporary_name, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow::anyhow!(
        "could not allocate a private temporary attachment file"
    ))
}

#[cfg(unix)]
fn cleanup_temporary_download<U, S>(
    directory: &std::fs::File,
    temporary_name: &std::ffi::CStr,
    unlink_temporary: &mut U,
    sync_directory: &mut S,
) -> std::io::Result<()>
where
    U: FnMut(&std::fs::File, &std::ffi::CStr) -> std::io::Result<()>,
    S: FnMut(&std::fs::File) -> std::io::Result<()>,
{
    unlink_temporary(directory, temporary_name)?;
    sync_directory(directory)
}

#[cfg(unix)]
fn cleanup_temporary_download_error<U, S>(
    original: anyhow::Error,
    directory: &std::fs::File,
    temporary_name: &std::ffi::CStr,
    unlink_temporary: &mut U,
    sync_directory: &mut S,
) -> anyhow::Error
where
    U: FnMut(&std::fs::File, &std::ffi::CStr) -> std::io::Result<()>,
    S: FnMut(&std::fs::File) -> std::io::Result<()>,
{
    match cleanup_temporary_download(directory, temporary_name, unlink_temporary, sync_directory) {
        Ok(()) => original,
        Err(cleanup_error) => {
            anyhow::anyhow!("{original}; a hidden temporary attachment may remain: {cleanup_error}")
        }
    }
}

#[cfg(unix)]
fn cleanup_published_download_error<U, S>(
    original: anyhow::Error,
    directory: &std::fs::File,
    final_name: &std::ffi::CStr,
    unlink_temporary: &mut U,
    sync_directory: &mut S,
) -> anyhow::Error
where
    U: FnMut(&std::fs::File, &std::ffi::CStr) -> std::io::Result<()>,
    S: FnMut(&std::fs::File) -> std::io::Result<()>,
{
    match cleanup_temporary_download(directory, final_name, unlink_temporary, sync_directory) {
        Ok(()) => original,
        Err(cleanup_error) => {
            anyhow::anyhow!("{original}; saved attachment may remain: {cleanup_error}")
        }
    }
}

#[cfg(unix)]
fn downloads_directory_matches(
    directory: &std::fs::File,
    downloads: &Path,
) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let pinned = directory.metadata()?;
    let Ok(current) = std::fs::metadata(downloads) else {
        return Ok(false);
    };
    Ok(pinned.dev() == current.dev() && pinned.ino() == current.ino())
}

#[cfg(unix)]
fn sync_private_download_directory(directory: &std::fs::File) -> std::io::Result<()> {
    directory.sync_all()
}

#[cfg(unix)]
enum PublishPrivateDownload {
    PublishedAndTemporaryRemoved,
    #[cfg(not(target_os = "macos"))]
    PublishedWithTemporary,
}

#[cfg(target_os = "macos")]
fn publish_private_download_at(
    directory: &std::fs::File,
    temporary_name: &std::ffi::CStr,
    final_name: &std::ffi::CStr,
) -> std::io::Result<PublishPrivateDownload> {
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            temporary_name.as_ptr(),
            directory.as_raw_fd(),
            final_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PublishPrivateDownload::PublishedAndTemporaryRemoved)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn publish_private_download_at(
    directory: &std::fs::File,
    temporary_name: &std::ffi::CStr,
    final_name: &std::ffi::CStr,
) -> std::io::Result<PublishPrivateDownload> {
    let result = unsafe {
        libc::linkat(
            directory.as_raw_fd(),
            temporary_name.as_ptr(),
            directory.as_raw_fd(),
            final_name.as_ptr(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PublishPrivateDownload::PublishedWithTemporary)
}

#[cfg(unix)]
fn open_private_download_at(
    directory: &std::fs::File,
    name: &std::ffi::CStr,
) -> std::io::Result<std::fs::File> {
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn unlink_private_download_at(
    directory: &std::fs::File,
    name: &std::ffi::CStr,
) -> std::io::Result<()> {
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn open_private_download_file(candidate: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(candidate)
}

/// Test-only operation seams for create/open fault injection. Unix production
/// saves use the descriptor-relative helper above, so path swaps cannot affect
/// attachment creation or cleanup.
#[cfg_attr(unix, allow(dead_code))]
fn save_private_download_with_operations<D, O, W, Q>(
    downloads: &Path,
    filename: &str,
    bytes: &[u8],
    mut create_downloads: D,
    mut open_new: O,
    mut write_and_sync: W,
    mut quarantine: Q,
) -> anyhow::Result<PathBuf>
where
    D: FnMut(&Path) -> std::io::Result<()>,
    O: FnMut(&Path) -> std::io::Result<std::fs::File>,
    W: FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    Q: FnMut(&std::fs::File) -> anyhow::Result<()>,
{
    if !is_safe_download_filename(filename) {
        anyhow::bail!("attachment filename is not a safe basename");
    }
    create_downloads(downloads)?;
    for counter in 0..10_000 {
        let candidate = downloads.join(download_name(filename, counter));
        match open_new(&candidate) {
            Ok(mut file) => {
                if let Err(write_error) = write_and_sync(&mut file, bytes) {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(write_error.into());
                }
                #[cfg(unix)]
                if let Err(permission_error) =
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))
                {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(permission_error.into());
                }
                if let Err(quarantine_error) = quarantine(&file) {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(quarantine_error);
                }
                return Ok(candidate);
            }
            Err(open_error) if open_error.kind() == ErrorKind::AlreadyExists => continue,
            Err(open_error) => return Err(open_error.into()),
        }
    }
    Err(anyhow::anyhow!(
        "could not choose a safe filename in Downloads"
    ))
}

fn is_safe_download_filename(filename: &str) -> bool {
    let mut components = Path::new(filename).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Mark the opened inode rather than following a path that could have been
/// replaced since create_new. Email downloads must keep Gatekeeper provenance.
#[cfg(target_os = "macos")]
fn quarantine_download(file: &std::fs::File) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let value = format!("0081;{timestamp:x};Dakia;");
    // The name is a static NUL-terminated string and the value is valid for
    // its explicit length. The descriptor remains owned by file throughout.
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn quarantine_download(_file: &std::fs::File) -> anyhow::Result<()> {
    Ok(())
}

fn download_name(filename: &str, counter: usize) -> String {
    if counter == 0 {
        return filename.to_owned();
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let extension = path.extension().and_then(|value| value.to_str());
    match extension {
        Some(extension) if !extension.is_empty() => format!("{stem} ({counter}).{extension}"),
        _ => format!("{stem} ({counter})"),
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn eml_export_filename_sanitizes_subjects_and_has_a_safe_fallback() {
        assert_eq!(
            eml_export_filename("Quarterly report: Tallinn"),
            "Quarterly report Tallinn.eml"
        );
        assert_eq!(eml_export_filename("../../\r\n"), "message.eml");
    }

    #[test]
    fn eml_export_filename_stays_within_the_filesystem_byte_limit() {
        let filename = eml_export_filename(&"€".repeat(180));

        assert!(filename.len() <= MAX_EXPORT_FILENAME_BYTES);
        assert!(filename.ends_with(".eml"));
        assert_eq!(filename.trim_end_matches(".eml").len(), 180);
        assert!(
            download_name(&filename, 9999).len() <= MAX_EXPORT_FILENAME_BYTES,
            "the longest collision suffix must still fit"
        );
    }

    #[test]
    fn private_download_keeps_bytes_and_chooses_a_unique_eml_name() {
        let directory = tempdir().expect("temporary Downloads directory");
        let raw = b"From: sender@example.test\r\nSubject: folded\r\n value\r\n\r\nopaque\0\xff";
        let first =
            save_private_download(directory.path(), "status.eml", raw).expect("first export");
        let second = save_private_download(directory.path(), "status.eml", b"second")
            .expect("second export");

        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("status.eml")
        );
        assert_eq!(
            second.file_name().and_then(|name| name.to_str()),
            Some("status (1).eml")
        );
        assert_eq!(std::fs::read(&first).expect("first export bytes"), raw);
        assert_eq!(
            std::fs::read(&second).expect("second export bytes"),
            b"second"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&first)
                .expect("first export metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn export_identity_rejects_uid_reuse_after_a_mailbox_epoch_change() {
        let before = MailSummary {
            mailbox_uid_validity: None,
            id: "message".into(),
            account_id: Uuid::nil().to_string(),
            mailbox: "INBOX".into(),
            uid: 42,
            message_id: Some("<old@example.test>".into()),
            gmail_message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: "thread".into(),
            subject: "Old".into(),
            from_name: None,
            from_address: "old@example.test".into(),
            to_addresses: "me@example.test".into(),
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
            attachments: vec![],
        };
        let mut reused_uid = before.clone();
        reused_uid.message_id = Some("<new@example.test>".into());
        reused_uid.received_at += chrono::Duration::seconds(1);

        assert!(same_export_identity(&before, &before));
        assert!(!same_export_identity(&before, &reused_uid));
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Eq, PartialEq)]
struct MacosAppMenuQuitSpec {
    id: &'static str,
    text: &'static str,
    accelerator: &'static str,
}

#[cfg(target_os = "macos")]
fn macos_app_menu_quit_spec() -> MacosAppMenuQuitSpec {
    MacosAppMenuQuitSpec {
        id: APP_MENU_QUIT_ID,
        text: "Quit Dakia",
        accelerator: "CmdOrCtrl+Q",
    }
}

#[cfg(target_os = "macos")]
fn build_macos_app_quit_item<R: tauri::Runtime>(
    app: &tauri::App<R>,
) -> tauri::Result<tauri::menu::MenuItem<R>> {
    let spec = macos_app_menu_quit_spec();
    MenuItemBuilder::with_id(spec.id, spec.text)
        .accelerator(spec.accelerator)
        .build(app)
}

fn install_app_menu(app: &tauri::App) -> tauri::Result<()> {
    let settings = MenuItemBuilder::with_id("settings", "Settings…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let check_for_updates =
        MenuItemBuilder::with_id("check-for-updates", "Check for Updates…").build(app)?;
    let new_message = MenuItemBuilder::with_id("new-message", "New Message")
        .accelerator("CmdOrCtrl+N")
        .build(app)?;
    let add_account = MenuItemBuilder::with_id("add-account", "Add Account…").build(app)?;
    let search = MenuItemBuilder::with_id("search", "Find in Mailbox")
        .accelerator("CmdOrCtrl+F")
        .build(app)?;
    let sync = MenuItemBuilder::with_id("sync", "Get New Mail")
        .accelerator("CmdOrCtrl+Shift+N")
        .build(app)?;
    let reply = MenuItemBuilder::with_id("reply", "Reply")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let forward = MenuItemBuilder::with_id("forward", "Forward")
        .accelerator("CmdOrCtrl+Shift+F")
        .build(app)?;
    let archive = MenuItemBuilder::with_id("archive", "Archive")
        .accelerator("CmdOrCtrl+Shift+A")
        .build(app)?;
    let spam = MenuItemBuilder::with_id("spam", "Mark as Junk")
        .accelerator("CmdOrCtrl+Shift+J")
        .build(app)?;
    let keyboard_shortcuts =
        MenuItemBuilder::with_id("keyboard-shortcuts", "Keyboard Shortcuts").build(app)?;
    #[cfg(target_os = "macos")]
    let app_quit = build_macos_app_quit_item(app)?;
    #[cfg(target_os = "macos")]
    let terminal_command = {
        let label = app
            .path()
            .resource_dir()
            .ok()
            .and_then(|_| std::env::current_exe().ok())
            .and_then(|path| path.parent().map(|parent| parent.join("dakia")))
            .filter(|path| {
                matches!(
                    terminal_command_status_for(path),
                    TerminalCommandStatus::Available
                )
            })
            .map(|_| "Remove Dakia Terminal Command…")
            .unwrap_or("Use Dakia from Terminal…");
        MenuItemBuilder::with_id("terminal-command", label).build(app)?
    };

    let app_menu_builder = SubmenuBuilder::new(app, "Dakia")
        .about(None)
        .separator()
        .item(&settings)
        .item(&check_for_updates);
    #[cfg(target_os = "macos")]
    let app_menu_builder = app_menu_builder.separator().item(&terminal_command);
    #[cfg(target_os = "macos")]
    let app_menu = app_menu_builder
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(&app_quit)
        .build()?;
    #[cfg(not(target_os = "macos"))]
    let app_menu = app_menu_builder
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;
    let file_menu = SubmenuBuilder::new(app, "File")
        .item(&new_message)
        .item(&add_account)
        .separator()
        .close_window()
        .build()?;
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let mailbox_menu = SubmenuBuilder::new(app, "Mailbox")
        .item(&sync)
        .separator()
        .item(&search)
        .build()?;
    let message_menu = SubmenuBuilder::new(app, "Message")
        .item(&reply)
        .item(&forward)
        .separator()
        .item(&archive)
        .item(&spam)
        .build()?;
    let window_menu = SubmenuBuilder::with_id(app, "window", "Window")
        .minimize()
        .maximize()
        .fullscreen()
        .separator()
        .bring_all_to_front()
        .build()?;
    let help_menu = SubmenuBuilder::new(app, "Help")
        .item(&keyboard_shortcuts)
        .build()?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &app_menu,
            &file_menu,
            &edit_menu,
            &mailbox_menu,
            &message_menu,
            &window_menu,
            &help_menu,
        ])
        .build()?;
    app.set_menu(menu)?;
    Ok(())
}

fn install_tray(app: &tauri::AppHandle, open_label: &str, quit_label: &str) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("tray-open", open_label).build(app)?;
    let quit = MenuItemBuilder::with_id("tray-quit", quit_label).build(app)?;
    let menu = MenuBuilder::new(app).items(&[&open, &quit]).build()?;
    if let Some(tray) = app.tray_by_id("dakia-tray") {
        tray.set_menu(Some(menu))?;
        return Ok(());
    }
    let tray_icon = Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
    let builder = TrayIconBuilder::with_id("dakia-tray")
        .icon(tray_icon)
        .icon_as_template(true)
        .tooltip("Dakia")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-open" => show_main_window(app),
            "tray-quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    app.state::<Arc<AppState>>().realtime.stop_all().await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    builder.build(app)?;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn menu_action_targets_focused_window(action: &str) -> bool {
    matches!(action, "reply" | "forward" | "archive" | "spam")
        || action.starts_with("copy-email-address:")
        || action.starts_with("compose-email-address:")
}

#[tauri::command]
async fn provider_presets() -> Vec<ProviderPreset> {
    provider::all().to_vec()
}

#[tauri::command]
fn configure_tray(
    app: tauri::AppHandle,
    open_label: String,
    quit_label: String,
) -> Result<(), String> {
    if open_label.trim().is_empty() || quit_label.trim().is_empty() {
        return Err("Tray labels are required".into());
    }
    install_tray(&app, open_label.trim(), quit_label.trim()).map_err(error)
}

#[tauri::command]
async fn accounts(state: State<'_, Arc<AppState>>) -> Result<Vec<Account>, String> {
    state.store.accounts().await.map_err(error)
}

#[tauri::command]
async fn update_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    input: UpdateAccountInput,
) -> Result<Account, String> {
    if input.account_name.trim().is_empty() {
        return Err("Account name is required".into());
    }
    if input.display_name.trim().is_empty() {
        return Err("Your name is required".into());
    }
    if input.imap_host.trim().is_empty() || input.smtp_host.trim().is_empty() {
        return Err("IMAP and SMTP hosts are required".into());
    }
    // Ask an existing rebuild to stop before waiting for its account lock. A
    // normal settings or credential update retains the durable job so it can
    // resume with the new connection details.
    request_mail_rebuild_cancel(
        state.inner(),
        input.id,
        MailRebuildCancellationDisposition::Retain,
    );
    let _operation = state.account_operations.acquire(input.id).await;
    let mut account = state
        .store
        .account(input.id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    let previous_account = account.clone();
    account.account_name = input.account_name.trim().to_owned();
    account.display_name = input.display_name.trim().to_owned();
    account.imap_host = input.imap_host.trim().to_owned();
    account.imap_port = input.imap_port;
    account.imap_security = input.imap_security;
    account.smtp_host = input.smtp_host.trim().to_owned();
    account.smtp_port = input.smtp_port;
    account.smtp_security = input.smtp_security;
    account.archive_mailbox = input.archive_mailbox.trim().to_owned();
    account.spam_mailbox = input.spam_mailbox.trim().to_owned();
    let password = input.password.filter(|value| !value.trim().is_empty());
    let password_was_supplied = password.is_some();
    let converts_legacy_oauth =
        password_was_supplied && matches!(&previous_account.auth, AccountAuth::OAuth2 { .. });
    if converts_legacy_oauth {
        if let Err(validation_error) =
            validate_legacy_oauth_conversion(&previous_account, &state.realtime.statuses().await)
        {
            resume_scheduled_mail_rebuild(
                app.clone(),
                state.inner().clone(),
                previous_account.clone(),
            )
            .await;
            return Err(validation_error);
        }
    } else if password_was_supplied && !matches!(account.auth, AccountAuth::Password { .. }) {
        resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account.clone())
            .await;
        return Err("OAuth accounts can only be converted after authentication fails".into());
    }
    let namespace_changed = !same_mail_namespace(&previous_account, &account);
    if converts_legacy_oauth && namespace_changed {
        resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account.clone())
            .await;
        return Err(
            "Save the Google app password before changing the IMAP or folder settings".into(),
        );
    }
    if namespace_changed || converts_legacy_oauth {
        // Stop the old namespace watcher before committing a replacement.
        state.realtime.stop_account(account.id).await;
    }
    let password_secret_name = credential_secret_name(&account);

    if converts_legacy_oauth {
        convert_legacy_oauth_to_password(&mut account);
        if let Err(save_error) = state
            .store
            .save_account_with_secret(
                &account,
                &password_secret_name,
                password.as_deref().expect("conversion password"),
            )
            .await
        {
            if previous_account.enabled {
                state
                    .realtime
                    .start_account(app.clone(), previous_account.clone())
                    .await;
            }
            resume_scheduled_mail_rebuild(app.clone(), state.inner().clone(), previous_account)
                .await;
            return Err(error(save_error));
        }
        if account.enabled {
            state
                .realtime
                .start_account(app.clone(), account.clone())
                .await;
        }
        resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
        return Ok(account);
    }

    let previous_password_credential = if password_was_supplied {
        match state.store.secret(&password_secret_name).await {
            Ok(credential) => credential,
            Err(secret_error) => {
                if namespace_changed {
                    let _ = state.realtime.reconcile(app.clone()).await;
                }
                return Err(error(secret_error));
            }
        }
    } else {
        None
    };
    if let Some(password) = password.as_deref() {
        if let Err(set_error) = MailService::new(state.store.clone())
            .credentials()
            .set_password(&account, password)
            .await
        {
            if namespace_changed {
                let _ = state.realtime.reconcile(app.clone()).await;
            }
            resume_scheduled_mail_rebuild(
                app.clone(),
                state.inner().clone(),
                previous_account.clone(),
            )
            .await;
            return Err(error(set_error));
        }
    }
    if let Err(save_error) = save_account_with_rebuild_intent(
        state.inner(),
        &account,
        namespace_changed,
        None,
        &password_secret_name,
    )
    .await
    {
        if password_was_supplied {
            let rollback_mail = MailService::new(state.store.clone());
            let credentials = rollback_mail.credentials();
            let rollback = match previous_password_credential {
                Some(previous) => {
                    state
                        .store
                        .set_secret(&password_secret_name, &previous)
                        .await
                }
                None => credentials.delete(&account).await,
            };
            if let Err(rollback_error) = rollback {
                tracing::error!(account_id = %account.id, error = %rollback_error, "could not roll back password credentials after saving the account failed");
            }
        }
        if namespace_changed {
            let _ = state.realtime.reconcile(app.clone()).await;
        }
        resume_scheduled_mail_rebuild(app, state.inner().clone(), previous_account).await;
        return Err(error(save_error));
    }
    if !namespace_changed {
        state.realtime.reconcile(app.clone()).await.map_err(error)?;
    }
    resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
    Ok(account)
}

#[tauri::command]
async fn show_account_context_menu(
    window: tauri::Window,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    rename_label: String,
) -> Result<(), String> {
    if state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .is_none()
    {
        return Err("Account not found".into());
    }
    let rename = MenuItemBuilder::with_id(format!("rename-account:{account_id}"), rename_label)
        .build(&window)
        .map_err(error)?;
    let menu = MenuBuilder::new(&window)
        .item(&rename)
        .build()
        .map_err(error)?;
    window.popup_menu(&menu).map_err(error)
}

fn validated_context_menu_email_address(value: &str) -> Result<&str, String> {
    let address = value.trim();
    if address.is_empty()
        || address.len() > 320
        || address.matches('@').count() != 1
        || address
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("Invalid email address".into());
    }
    Ok(address)
}

fn decode_context_menu_email_address(value: &str) -> Result<String, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "Invalid encoded email address".to_owned())?;
    let address =
        String::from_utf8(bytes).map_err(|_| "Invalid encoded email address".to_owned())?;
    validated_context_menu_email_address(&address)?;
    Ok(address)
}

#[tauri::command]
fn show_email_address_context_menu(
    window: tauri::Window,
    account_id: Uuid,
    address: String,
    copy_label: String,
    new_message_label: String,
) -> Result<(), String> {
    let address = validated_context_menu_email_address(&address)?;
    let encoded_address = URL_SAFE_NO_PAD.encode(address.as_bytes());
    let copy =
        MenuItemBuilder::with_id(format!("copy-email-address:{encoded_address}"), copy_label)
            .build(&window)
            .map_err(error)?;
    let new_message = MenuItemBuilder::with_id(
        format!("compose-email-address:{account_id}:{encoded_address}"),
        new_message_label,
    )
    .build(&window)
    .map_err(error)?;
    let menu = MenuBuilder::new(&window)
        .items(&[&copy, &new_message])
        .build()
        .map_err(error)?;
    window.popup_menu(&menu).map_err(error)
}

#[cfg(test)]
mod email_address_context_menu_tests {
    use super::*;

    #[test]
    fn validates_email_addresses_before_building_menu_ids() {
        assert_eq!(
            validated_context_menu_email_address("  person+tag@example.com  "),
            Ok("person+tag@example.com")
        );
        assert_eq!(
            validated_context_menu_email_address("müller@example.com"),
            Ok("müller@example.com")
        );
        for invalid in [
            "",
            "missing-at.example.com",
            "two@@example.com",
            "person @example.com",
            "person@example.com\tmenu-action",
            "person@example.com\nmenu-action",
            "person@example.com\u{0000}",
        ] {
            assert!(
                validated_context_menu_email_address(invalid).is_err(),
                "accepted invalid address {invalid:?}"
            );
        }

        let maximum = format!("{}@b", "a".repeat(318));
        let too_long = format!("{}@b", "a".repeat(319));
        assert_eq!(maximum.len(), 320);
        assert!(validated_context_menu_email_address(&maximum).is_ok());
        assert!(validated_context_menu_email_address(&too_long).is_err());
    }

    #[test]
    fn menu_address_encoding_is_url_safe_and_round_trips_utf8() {
        let address = "müller+news@example.com";
        let encoded = URL_SAFE_NO_PAD.encode(address.as_bytes());
        assert!(encoded
            .chars()
            .all(|character| !matches!(character, '+' | '/' | '=')));
        assert_eq!(URL_SAFE_NO_PAD.decode(encoded).unwrap(), address.as_bytes());
        assert_eq!(
            decode_context_menu_email_address(&URL_SAFE_NO_PAD.encode(address)),
            Ok(address.to_owned())
        );
        assert!(decode_context_menu_email_address("_w").is_err());
        assert!(
            decode_context_menu_email_address(&URL_SAFE_NO_PAD.encode("not@an@address")).is_err()
        );
    }

    #[test]
    fn address_actions_return_to_the_window_that_opened_the_menu() {
        assert!(menu_action_targets_focused_window(
            "copy-email-address:cGVyc29uQGV4YW1wbGUuY29t"
        ));
        assert!(menu_action_targets_focused_window(
            "compose-email-address:account:cGVyc29uQGV4YW1wbGUuY29t"
        ));
        assert!(menu_action_targets_focused_window("reply"));
        assert!(!menu_action_targets_focused_window("new-message"));
        assert!(!menu_action_targets_focused_window(
            "rename-account:account"
        ));
    }
}

#[tauri::command]
async fn remove_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
) -> Result<(), String> {
    request_mail_rebuild_cancel(
        state.inner(),
        account_id,
        MailRebuildCancellationDisposition::Remove,
    );
    let _operation = state.account_operations.acquire(account_id).await;
    state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    // `stop_account` waits for the watcher task to leave IMAP and complete
    // its current storage call before destructive storage work begins.
    state.realtime.stop_account(account_id).await;
    state
        .store
        .delete_account_with_credential(account_id)
        .await
        .map_err(error)?;
    if let Err(error) = app.emit(
        "account-removed",
        serde_json::json!({ "accountId": account_id }),
    ) {
        tracing::error!(error = %error, "could not notify windows about account removal");
    }
    if let Err(error) = state.realtime.reconcile(app).await {
        tracing::error!(error = %error, "could not reconcile real-time mail after account removal");
    }
    Ok(())
}

#[tauri::command]
fn open_external_url<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    url: String,
) -> Result<(), String> {
    validate_external_url(&url)?;
    app.opener().open_url(url, None::<&str>).map_err(error)
}

fn validate_external_url(url: &str) -> Result<(), String> {
    if url.trim() != url || url.chars().any(char::is_control) {
        return Err("This link contains invalid characters".into());
    }
    let parsed = Url::parse(url).map_err(|_| "This link is not valid".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return Err("Only web and email links can be opened".into());
    }
    if matches!(parsed.scheme(), "http" | "https")
        && (parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some())
    {
        return Err("Web links must have a host and cannot contain login details".into());
    }
    Ok(())
}

struct MailWindowSpec {
    label: String,
    path: String,
    title: &'static str,
    size: (f64, f64),
    minimum_size: (f64, f64),
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn validate_native_mail_safety_data_store_identifier(
    fixture_marker: Option<&str>,
    root_is_marked: bool,
    configured_identifier: Option<[u8; 16]>,
) -> anyhow::Result<Option<[u8; 16]>> {
    let Some(identifier) = configured_identifier else {
        return Ok(None);
    };
    let Some(fixture_marker) = fixture_marker else {
        anyhow::bail!("native mail-safety data store requires the exact fixture marker");
    };
    if fixture_marker != "1" {
        anyhow::bail!("native mail-safety fixture marker must equal 1");
    }
    if !root_is_marked {
        anyhow::bail!("native mail-safety data store requires its exact owned fixture root");
    }
    let uuid = Uuid::from_bytes(identifier);
    if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
        anyhow::bail!("native mail-safety data store must use a random UUID");
    }
    Ok(Some(identifier))
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn parse_native_mail_safety_data_store_identifier(value: &str) -> anyhow::Result<[u8; 16]> {
    let identifier = Uuid::parse_str(value)
        .map_err(|_| anyhow::anyhow!("native mail-safety data store UUID is invalid"))?;
    if identifier.to_string() != value {
        anyhow::bail!("native mail-safety data store UUID must be canonical");
    }
    Ok(*identifier.as_bytes())
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn native_mail_safety_data_store_identifier_from_environment() -> anyhow::Result<Option<[u8; 16]>> {
    let configured_identifier =
        match std::env::var("DAKIA_NATIVE_MAIL_SAFETY_DATA_STORE_IDENTIFIER") {
            Ok(value) => Some(parse_native_mail_safety_data_store_identifier(&value)?),
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
    let fixture_marker = match std::env::var("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let fixture_is_marked = if fixture_marker.as_deref() == Some("1") {
        let root = std::env::var("DAKIA_NATIVE_MAIL_SAFETY_ROOT")
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("DAKIA_NATIVE_MAIL_SAFETY_ROOT is required"))?;
        if !root.is_absolute() {
            anyhow::bail!("native mail-safety fixture root must be absolute");
        }
        let root = root.canonicalize()?;
        if std::fs::read_to_string(root.join(".dakia-native-mail-safety-root"))
            .ok()
            .as_deref()
            != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
        {
            anyhow::bail!("native mail-safety fixture root lacks its exact ownership sentinel");
        }
        let profile = std::env::var("DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR")
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR is required"))?;
        native_mail_safety_profile_directory(&root, profile)?;
        true
    } else {
        false
    };
    validate_native_mail_safety_data_store_identifier(
        fixture_marker.as_deref(),
        fixture_is_marked,
        configured_identifier,
    )
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn set_native_mail_safety_main_data_store_identifier(
    windows: &mut [tauri::utils::config::WindowConfig],
    identifier: Option<[u8; 16]>,
) -> anyhow::Result<()> {
    let mut main_windows = windows.iter_mut().filter(|window| window.label == "main");
    let main = main_windows
        .next()
        .ok_or_else(|| anyhow::anyhow!("native mail-safety fixture config has no main window"))?;
    if main_windows.next().is_some() {
        anyhow::bail!("native mail-safety fixture config contains duplicate main windows");
    }
    if main.data_store_identifier.is_some() {
        anyhow::bail!("native mail-safety fixture config must omit a static data-store identifier");
    }
    main.data_store_identifier = identifier;
    Ok(())
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn apply_native_mail_safety_data_store_identifier<R: tauri::Runtime>(
    context: &mut tauri::Context<R>,
) -> anyhow::Result<()> {
    let identifier = native_mail_safety_data_store_identifier_from_environment()?;
    set_native_mail_safety_main_data_store_identifier(
        &mut context.config_mut().app.windows,
        identifier,
    )
}

#[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
fn native_mail_safety_data_store_identifier<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> anyhow::Result<Option<[u8; 16]>> {
    let expected = native_mail_safety_data_store_identifier_from_environment()?;
    let mut main_windows = app
        .config()
        .app
        .windows
        .iter()
        .filter(|window| window.label == "main");
    let configured = main_windows
        .next()
        .ok_or_else(|| anyhow::anyhow!("native mail-safety fixture config has no main window"))?
        .data_store_identifier;
    if main_windows.next().is_some() {
        anyhow::bail!("native mail-safety fixture config contains duplicate main windows");
    }
    if configured != expected {
        anyhow::bail!("native mail-safety data-store configuration changed after startup");
    }
    Ok(expected)
}

fn open_mail_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    spec: MailWindowSpec,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&spec.label) {
        window
            .show()
            .and_then(|_| window.set_focus())
            .map_err(error)?;
        return Ok(());
    }
    let builder = WebviewWindowBuilder::new(app, spec.label, WebviewUrl::App(spec.path.into()));
    #[cfg(all(feature = "native-mail-safety-fixture", target_os = "macos"))]
    let builder = match native_mail_safety_data_store_identifier(app).map_err(error)? {
        Some(identifier) => builder.data_store_identifier(identifier),
        None => builder,
    };
    builder
        .title(spec.title)
        .inner_size(spec.size.0, spec.size.1)
        .min_inner_size(spec.minimum_size.0, spec.minimum_size.1)
        .center()
        .focused(true)
        .resizable(true)
        .decorations(true)
        .shadow(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .build()
        .map(|_| ())
        .map_err(error)
}

fn window_seed_key(seed_key: &str) -> Result<(), String> {
    if seed_key.len() != 36 || Uuid::parse_str(seed_key).is_err() {
        return Err("Window seed key is invalid".into());
    }
    Ok(())
}

fn is_reader_window_label(label: &str) -> bool {
    label.len() == "reader-".len() + 32
        && label.strip_prefix("reader-").is_some_and(|digest| {
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn compose_opener_label<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<String, String> {
    let label = window.label();
    if label == "main" || is_reader_window_label(label) {
        Ok(label.to_string())
    } else {
        Err("Compose windows may only be opened by the main or Reader window".into())
    }
}

#[tauri::command]
fn open_compose_window<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    window: tauri::WebviewWindow<R>,
    seed_key: Option<String>,
    seed_db_key: Option<String>,
) -> Result<String, String> {
    let opener = compose_opener_label(&window)?;
    let seed = match (seed_key, seed_db_key) {
        (Some(seed_key), None) => {
            window_seed_key(&seed_key)?;
            ("seedKey", seed_key)
        }
        (None, Some(seed_db_key)) => {
            window_seed_key(&seed_db_key)?;
            ("seedDbKey", seed_db_key)
        }
        _ => return Err("Compose windows require exactly one draft seed key".into()),
    };
    let label = format!("compose-{}", Uuid::new_v4());
    // The caller label is derived from the authenticated WebviewWindow, not
    // from IPC input. Both permitted labels have a restricted ASCII form.
    let path = format!("/?view=compose&{}={}&opener={opener}", seed.0, seed.1);
    open_mail_window(
        &app,
        MailWindowSpec {
            label: label.clone(),
            path,
            title: "New message",
            size: (880.0, 700.0),
            minimum_size: (620.0, 480.0),
        },
    )?;
    Ok(label)
}

#[tauri::command]
fn open_reader_window<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    label: String,
    seed_key: Option<String>,
    seed_db_key: Option<String>,
) -> Result<(), String> {
    if !is_reader_window_label(&label) {
        return Err("Reader window label is invalid".into());
    }
    let path = reader_window_path(seed_key, seed_db_key)?;
    open_mail_window(
        &app,
        MailWindowSpec {
            label,
            path,
            title: "Dakia",
            size: (980.0, 760.0),
            minimum_size: (600.0, 480.0),
        },
    )
}

fn reader_window_path(
    seed_key: Option<String>,
    seed_db_key: Option<String>,
) -> Result<String, String> {
    match (seed_key, seed_db_key) {
        (Some(seed_key), None) => {
            window_seed_key(&seed_key)?;
            Ok(format!("/?view=reader&seedKey={seed_key}"))
        }
        (None, Some(seed_db_key)) => {
            window_seed_key(&seed_db_key)?;
            Ok(format!("/?view=reader&seedDbKey={seed_db_key}"))
        }
        _ => Err("Reader windows require exactly one target seed key".into()),
    }
}

#[tauri::command]
fn open_settings_window<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    account_id: Option<Uuid>,
) -> Result<(), String> {
    let suffix = account_id
        .map(|account_id| format!("&accountId={account_id}"))
        .unwrap_or_default();
    open_mail_window(
        &app,
        MailWindowSpec {
            label: "settings".into(),
            path: format!("/?view=settings{suffix}"),
            title: "Settings",
            size: (760.0, 560.0),
            minimum_size: (640.0, 460.0),
        },
    )
}

#[tauri::command]
fn open_account_window<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    open_mail_window(
        &app,
        MailWindowSpec {
            label: "account".into(),
            path: "/?view=account".into(),
            title: "Add Account",
            size: (620.0, 720.0),
            minimum_size: (520.0, 560.0),
        },
    )
}

#[tauri::command]
async fn add_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    input: AddAccountInput,
) -> Result<AccountConnection, String> {
    if input.password.trim().is_empty() {
        return Err("A password or app password is required".into());
    }
    let preset = input
        .draft
        .provider_id
        .as_deref()
        .and_then(provider::by_id)
        .unwrap_or_else(|| provider::detect(&input.draft.email));
    if preset.id == "custom"
        && (input
            .draft
            .imap_host
            .as_deref()
            .unwrap_or_default()
            .is_empty()
            || input
                .draft
                .smtp_host
                .as_deref()
                .unwrap_or_default()
                .is_empty())
    {
        return Err("Custom accounts require IMAP and SMTP hosts".into());
    }
    let account = input.draft.into_account(preset);
    let stored_accounts = state.store.accounts().await.map_err(error)?;
    ensure_account_is_not_connected(&stored_accounts, &account.email)?;
    let _operation = state.account_operations.acquire(account.id).await;
    state.realtime.stop_account(account.id).await;
    let mail = MailService::new(state.store.clone());
    let password_secret_name = credential_secret_name(&account);
    if let Err(set_error) = mail
        .credentials()
        .set_password(&account, &input.password)
        .await
    {
        return Err(error(set_error));
    }
    if let Err(save_error) =
        save_account_with_rebuild_intent(state.inner(), &account, true, None, &password_secret_name)
            .await
    {
        let rollback = mail.credentials().delete(&account).await;
        if let Err(rollback_error) = rollback {
            tracing::error!(
                account_id = %account.id,
                error = %rollback_error,
                "could not roll back password credentials after saving the account failed"
            );
        }
        return Err(error(save_error));
    }
    resume_scheduled_mail_rebuild(app, state.inner().clone(), account.clone()).await;
    Ok(AccountConnection {
        account,
        reused_existing_account: false,
    })
}

#[tauri::command]
async fn search(
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<MailConversationPage, String> {
    state
        .store
        .search_conversation_page(&query)
        .await
        .map_err(error)
}

#[tauri::command]
async fn search_smart_inbox(
    state: State<'_, Arc<AppState>>,
    query: SmartInboxQuery,
) -> Result<SmartInboxPage, String> {
    state.store.search_smart_inbox(&query).await.map_err(error)
}

#[tauri::command]
async fn conversation_for_target(
    state: State<'_, Arc<AppState>>,
    target: ConversationTarget,
) -> Result<Option<MailConversation>, String> {
    state
        .store
        .conversation_for_target(&target)
        .await
        .map_err(error)
}

#[tauri::command]
async fn search_remote(
    state: State<'_, Arc<AppState>>,
    query: SearchQuery,
) -> Result<RemoteSearchResult, String> {
    if query.text.trim().is_empty() {
        return Ok(RemoteSearchResult::default());
    }
    let limit = query.limit.unwrap_or(100).min(500) as usize;
    let search_state = state.inner().clone();
    let search_text = query.text.clone();
    let search_mailbox = query.mailbox.clone();
    let searches = run_bounded_ordered(
        query.account_ids.clone(),
        REMOTE_SEARCH_CONCURRENCY,
        state.remote_operation_slots.clone(),
        move |account_id| {
            let state = search_state.clone();
            let text = search_text.clone();
            let mailbox = search_mailbox.clone();
            async move {
                let account = state
                    .store
                    .account(account_id)
                    .await
                    .map_err(error)?
                    .ok_or_else(|| "Account not found".to_owned())?;
                MailService::new(state.store.clone())
                    .search_remote(&account, &text, mailbox.as_deref(), limit)
                    .await
                    .map_err(error)
            }
        },
    )
    .await;
    // Reassemble completion results in requested account order. This makes the
    // first returned failure deterministic and preserves stable tie ordering.
    let raw_results = searches.into_iter().collect::<Result<Vec<_>, _>>()?;
    Ok(shape_remote_search_results(&query, limit, raw_results))
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSearchResult {
    messages: Vec<MailSummary>,
    limit_reached: bool,
}

fn shape_remote_search_results(
    query: &SearchQuery,
    limit: usize,
    raw_results: Vec<Vec<MailSummary>>,
) -> RemoteSearchResult {
    // A capped provider response means there may be more remote matches even
    // if local filtering later hides every returned row.
    let mut limit_reached = limit > 0 && raw_results.iter().any(|hits| hits.len() >= limit);
    let mut messages = Vec::new();
    for hits in raw_results {
        for message in hits {
            if query.mailbox.is_none()
                && matches!(message.mailbox.split("::").next(), Some("Spam" | "Trash"))
            {
                continue;
            }
            if (!query.unread_only || !message.is_read)
                && (!query.flagged_only || message.is_flagged)
                && (!query.unflagged_only || !message.is_flagged)
            {
                messages.push(message);
            }
        }
    }
    limit_reached |= messages.len() > limit;
    messages.sort_by_key(|result| std::cmp::Reverse(result.received_at));
    messages.truncate(limit);
    RemoteSearchResult {
        messages,
        limit_reached,
    }
}

#[cfg(test)]
mod remote_search_limit_tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};

    fn query() -> SearchQuery {
        SearchQuery {
            text: "fixture".into(),
            account_ids: vec![],
            mailbox: None,
            from: None,
            unread_only: false,
            read_only: false,
            flagged_only: false,
            unflagged_only: false,
            category: None,
            limit: Some(500),
            cursor: None,
        }
    }

    fn messages(account_id: &str, mailbox: &str, count: usize) -> Vec<MailSummary> {
        (0..count)
            .map(|index| MailSummary {
                id: format!("{account_id}:{mailbox}:{index}"),
                account_id: account_id.into(),
                mailbox: mailbox.into(),
                uid: index as i64 + 1,
                mailbox_uid_validity: Some(77),
                message_id: Some(format!("<{account_id}-{index}@example.test>")),
                gmail_message_id: None,
                in_reply_to: None,
                reference_ids: None,
                thread_id: format!("{account_id}:{index}"),
                subject: "Fixture remote match".into(),
                from_name: None,
                from_address: "sender@example.test".into(),
                to_addresses: "reader@example.test".into(),
                cc_addresses: String::new(),
                bcc_addresses: String::new(),
                reply_to_addresses: String::new(),
                received_at: Utc::now() - ChronoDuration::seconds(index as i64),
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
                attachments: vec![],
            })
            .collect()
    }

    #[test]
    fn remote_search_reports_no_limit_for_499_raw_eligible_messages() {
        let result =
            shape_remote_search_results(&query(), 500, vec![messages("one", "INBOX", 499)]);

        assert_eq!(result.messages.len(), 499);
        assert!(!result.limit_reached);
        assert_eq!(serde_json::to_value(result).unwrap()["limitReached"], false);
    }

    #[test]
    fn remote_search_reports_a_raw_provider_cap_before_spam_filtering() {
        let mut capped = messages("one", "Spam", 480);
        capped.extend(messages("one", "INBOX", 20));

        let result = shape_remote_search_results(&query(), 500, vec![capped]);

        assert_eq!(result.messages.len(), 20);
        assert!(result.limit_reached);
    }

    #[test]
    fn remote_search_reports_a_global_merge_that_exceeds_the_cap() {
        let result = shape_remote_search_results(
            &query(),
            500,
            vec![messages("one", "INBOX", 251), messages("two", "INBOX", 250)],
        );

        assert_eq!(result.messages.len(), 500);
        assert!(result.limit_reached);
    }
}

#[tauri::command]
async fn set_message_category(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    category: String,
    expected_uid_validity: Option<i64>,
) -> Result<(), String> {
    let initial =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    let account_id = Uuid::parse_str(&initial.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let message =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    if !same_message_epoch_identity(&initial, &message) {
        return Err(message_identity_changed_error());
    }
    if !state
        .store
        .set_message_category_at_epoch(&message, &category)
        .await
        .map_err(error)?
    {
        return Err(message_identity_changed_error());
    }
    Ok(())
}

#[tauri::command]
async fn set_message_starred(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    starred: bool,
    expected_uid_validity: Option<i64>,
) -> Result<dakia_core::MailSummary, String> {
    let message =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let message = require_current_message_target(
        &state.store,
        account_id,
        &message.mailbox,
        message.uid as u32,
        Some(&message_id),
        expected_uid_validity,
    )
    .await?;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .set_flagged_at_epoch(
            &account,
            &message.mailbox,
            message.uid as u32,
            starred,
            expected_uid_validity.expect("target validated"),
        )
        .await
        .map_err(error)?;
    if !state
        .store
        .set_message_flagged_at_epoch(&message, starred)
        .await
        .map_err(error)?
    {
        return Err(message_identity_changed_error());
    }
    if starred {
        // The loader carries the captured epoch through its cache publication.
        // Ignore a fetch failure, but never fall back to a replacement row.
        let _ =
            load_message_content(state.inner(), &message.id, message.mailbox_uid_validity).await;
    }
    confirm_message_epoch(&state.store, &message).await
}

#[tauri::command]
async fn set_message_read(
    state: State<'_, Arc<AppState>>,
    message_id: String,
    read: bool,
    expected_uid_validity: Option<i64>,
) -> Result<(), String> {
    let message =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let message = require_current_message_target(
        &state.store,
        account_id,
        &message.mailbox,
        message.uid as u32,
        Some(&message_id),
        expected_uid_validity,
    )
    .await?;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .set_read_at_epoch(
            &account,
            &message.mailbox,
            message.uid as u32,
            read,
            expected_uid_validity.expect("target validated"),
        )
        .await
        .map_err(error)?;
    if !state
        .store
        .set_message_read_at_epoch(&message, read)
        .await
        .map_err(error)?
    {
        return Err(message_identity_changed_error());
    }
    Ok(())
}

#[tauri::command]
async fn starred_conversation_count(
    state: State<'_, Arc<AppState>>,
    account_ids: Vec<Uuid>,
) -> Result<u64, String> {
    state
        .store
        .starred_conversation_count(&account_ids)
        .await
        .map_err(error)
}

fn kick_classification(state: Arc<AppState>) -> u64 {
    let (generation, should_start) = state.classification.request();
    if should_start {
        tauri::async_runtime::spawn(async move {
            if let Err(error) = drain_pending_classifications(state).await {
                tracing::error!(error = %error, "background message classification failed");
            }
        });
    }
    generation
}

async fn classify_pending_batch(state: Arc<AppState>) -> anyhow::Result<usize> {
    let model_revision = state
        .classifier
        .lock()
        .map_err(|_| anyhow::anyhow!("email classifier lock is unavailable"))?
        .revision()
        .to_owned();
    state
        .store
        .claim_classification_revision(&state.classification_owner, &model_revision)
        .await?;
    let messages = state
        .store
        .messages_for_model_classification_batch(CLASSIFICATION_BATCH_SIZE)
        .await?;
    if messages.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = messages.iter().map(|message| message.id.clone()).collect();
    let known_correspondence = state
        .store
        .messages_from_known_correspondents(&messages)
        .await?;
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
    let classifier_state = state.clone();
    let classifications = tauri::async_runtime::spawn_blocking(move || {
        let mut classifier = classifier_state
            .classifier
            .lock()
            .map_err(|_| anyhow::anyhow!("email classifier lock is unavailable"))?;
        classifier.classify(&inputs)
    })
    .await
    .map_err(|error| anyhow::anyhow!("email classifier task failed: {error}"))??;
    validate_classification_output_count(ids.len(), classifications.len())?;
    let updates: Vec<ModelClassificationUpdate> = messages
        .iter()
        .zip(classifications)
        .map(|(message, result)| {
            ModelClassificationUpdate::from_message(
                message,
                result.category,
                result.confidence,
                known_correspondence.contains(&message.id),
                &state.classification_owner,
                &model_revision,
            )
        })
        .collect();
    let count = updates.len();
    let applied = state.store.apply_model_classifications(&updates).await?;
    validate_classification_apply_count(count, applied)
}

async fn drain_pending_classifications(state: Arc<AppState>) -> anyhow::Result<()> {
    let result = drain_pending_classifications_owned(state.clone()).await;
    let release = state
        .store
        .release_classification_revision(&state.classification_owner)
        .await;
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn drain_pending_classifications_owned(state: Arc<AppState>) -> anyhow::Result<()> {
    let mut classified = 0;
    loop {
        let generation = state.classification.next_generation();
        match retry_classification_batch({
            let state = state.clone();
            move || classify_pending_batch(state.clone())
        })
        .await
        {
            Ok(count) => {
                classified += count;
                if count == CLASSIFICATION_BATCH_SIZE {
                    continue;
                }
                if !state
                    .classification
                    .finish_generation(generation, classified)
                {
                    return Ok(());
                }
            }
            Err(error) => {
                let failure = error.to_string();
                if state.classification.fail_generation(generation, failure) {
                    continue;
                }
                return Err(error);
            }
        }
    }
}

async fn classify_pending_messages(state: Arc<AppState>) -> anyhow::Result<usize> {
    let generation = kick_classification(state.clone());
    state.classification.wait_for(generation).await
}

#[tauri::command]
async fn classify_pending(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    classify_pending_messages(state.inner().clone())
        .await
        .map_err(error)
}

#[tauri::command]
async fn start_realtime_sync(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    state.realtime.reconcile(app).await.map_err(error)
}

#[tauri::command]
async fn reconcile_realtime_sync(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    state.realtime.reconcile(app).await.map_err(error)
}

#[tauri::command]
async fn realtime_sync_status(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<RealtimeSyncStatus>, String> {
    Ok(state.realtime.statuses().await)
}

#[tauri::command]
fn record_notification_delivered(
    account_id: Uuid,
    event_id: Uuid,
    detected_at: String,
) -> Result<(), String> {
    let detected_at = chrono::DateTime::parse_from_rfc3339(&detected_at).map_err(error)?;
    let latency_ms = chrono::Utc::now()
        .signed_duration_since(detected_at.with_timezone(&chrono::Utc))
        .num_milliseconds()
        .max(0);
    tracing::info!(
        account_id = %account_id,
        event_id = %event_id,
        notification_latency_ms = latency_ms,
        "new mail notification delivered"
    );
    Ok(())
}

#[tauri::command]
async fn hydrate_message(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<dakia_core::MailSummary, String> {
    let message = hydrated_message(state.inner(), &message_id, expected_uid_validity).await?;
    let account_id = Uuid::parse_str(&message.account_id).map_err(error)?;
    kick_classification(state.inner().clone());
    let _ = app.emit(
        "mail-hydrated",
        serde_json::json!({
            "accountId": account_id,
            "messageId": message.id,
        }),
    );
    Ok(message)
}

async fn hydrated_message(
    state: &Arc<AppState>,
    message_id: &str,
    expected_uid_validity: Option<i64>,
) -> Result<MailSummary, String> {
    let content = load_message_content(state, message_id, expected_uid_validity).await?;
    let mut message =
        current_message_at_epoch(&state.store, message_id, expected_uid_validity).await?;
    message.body_text = content.body_text;
    message.body_html = content.body_html;
    message.unsubscribe_kind = content.unsubscribe_kind;
    message.content_state = "complete".into();
    message.attachments = content
        .attachments
        .into_iter()
        .map(|attachment| dakia_core::storage::AttachmentData {
            attachment,
            bytes: Vec::new(),
        })
        .collect();
    message.has_attachments = !message.attachments.is_empty();
    Ok(message)
}

fn publish_mail_rebuild_progress(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    account_id: Uuid,
    progress: SyncProgress,
) {
    let reset_before_sync = state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .get(&account_id)
        .map(|job| job.reset_before_sync)
        .unwrap_or(false);
    let update = MailRebuildProgress {
        account_id,
        phase: progress.phase.to_owned(),
        completed: progress.completed,
        total: progress.total,
        reset_before_sync,
    };
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account_id, update.clone());
    let _ = app.emit("mail-rebuild-progress", &update);
}

fn request_mail_rebuild_cancel(
    state: &Arc<AppState>,
    account_id: Uuid,
    disposition: MailRebuildCancellationDisposition,
) {
    state
        .mail_rebuild_cancellations
        .request(account_id, disposition);
}

fn reserve_mail_rebuild(state: &Arc<AppState>, account_id: Uuid) -> bool {
    let reserved = state
        .mail_rebuild_running
        .lock()
        .expect("mail rebuild reservation lock poisoned")
        .insert(account_id);
    if reserved {
        // Register the cancellation channel with the reservation, before the
        // spawned task can run. A namespace-changing update can now cancel a
        // queued worker rather than missing the pre-registration window.
        state.mail_rebuild_cancellations.reserve(account_id);
    }
    reserved
}

fn release_mail_rebuild(state: &Arc<AppState>, account_id: Uuid) {
    let released = state
        .mail_rebuild_running
        .lock()
        .expect("mail rebuild reservation lock poisoned")
        .remove(&account_id);
    if released {
        state
            .mail_rebuild_cancellations
            .release_reservation(account_id);
    }
}

async fn schedule_mail_rebuild(
    state: &Arc<AppState>,
    account_id: Uuid,
    reset_before_sync: bool,
) -> anyhow::Result<()> {
    let job = MailRebuildJob {
        account_id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync,
    };
    // Persistence comes first. A realtime reconcile must never win the race
    // with the durable replacement intent.
    state.store.save_mail_rebuild_job(&job).await?;
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account_id, job.into());
    Ok(())
}

fn reset_mail_rebuild_job(account_id: Uuid) -> MailRebuildJob {
    MailRebuildJob {
        account_id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync: true,
    }
}

async fn save_account_with_rebuild_intent(
    state: &Arc<AppState>,
    account: &Account,
    reset_before_sync: bool,
    previous_secret_name: Option<&str>,
    current_secret_name: &str,
) -> anyhow::Result<()> {
    if !reset_before_sync {
        return state.store.save_account(account).await;
    }
    let job = reset_mail_rebuild_job(account.id);
    // The namespace replacement is indivisible: a reconnect can never leave
    // a changed remote identity saved without its required reset job.
    if let Some(previous_secret_name) = previous_secret_name {
        state
            .store
            .save_account_with_reset_mail_rebuild_job_and_delete_previous_secret(
                account,
                &job,
                Some(previous_secret_name),
                current_secret_name,
            )
            .await?;
    } else {
        state
            .store
            .save_account_with_reset_mail_rebuild_job(account, &job)
            .await?;
    }
    state
        .mail_rebuilds
        .lock()
        .expect("mail rebuild lock poisoned")
        .insert(account.id, job.into());
    Ok(())
}

async fn resume_scheduled_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
) {
    let in_memory = {
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .get(&account.id)
            .map(|job| job.reset_before_sync)
    };
    let reset_before_sync = match in_memory {
        Some(reset_before_sync) => Some(reset_before_sync),
        None => match state.store.mail_rebuild_jobs().await {
            Ok(jobs) => jobs
                .into_iter()
                .find(|job| job.account_id == account.id)
                .map(|job| job.reset_before_sync),
            Err(error) => {
                tracing::warn!(account_id = %account.id, error = %error, "could not load retained mail rebuild job");
                None
            }
        },
    };
    let Some(reset_before_sync) = reset_before_sync else {
        return;
    };
    if !reserve_mail_rebuild(&state, account.id) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_mail_rebuild(app, state, account, reset_before_sync).await {
            tracing::warn!(error = %error, "could not resume retained mail rebuild");
        }
    });
}

fn should_retain_mail_rebuild_job(
    error: &anyhow::Error,
    disposition: MailRebuildCancellationDisposition,
    reset_before_sync: bool,
) -> bool {
    if matches!(
        disposition,
        MailRebuildCancellationDisposition::Retain | MailRebuildCancellationDisposition::Replace
    ) {
        return true;
    }
    if matches!(disposition, MailRebuildCancellationDisposition::Remove)
        || error.to_string() == "mail rebuild cancelled"
    {
        return false;
    }
    // A first or replacement index has no trusted catalogue yet. Keep its
    // reset intent even for a credential error so reconnecting the same
    // namespace can finish the original replacement instead of silently
    // downgrading to a normal incremental sync.
    if reset_before_sync {
        return true;
    }
    !error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("imap authentication rejected")
            || message.contains("credentials are not stored")
            || message.contains("stored oauth credentials are invalid")
    })
}

fn effective_rebuild_reset(requested_reset: bool, durable_reset: Option<bool>) -> bool {
    requested_reset || durable_reset.unwrap_or(false)
}

async fn run_mail_rebuild(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
) -> anyhow::Result<SyncResult> {
    // Register before waiting on the account lock. Update/remove can request
    // cancellation while this rebuild is queued behind another operation.
    let cancel_receiver = state.mail_rebuild_cancellations.register(account.id);
    let _operation = state.account_operations.acquire(account.id).await;
    let current_account = match state.store.account(account.id).await {
        Err(error) => {
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(error);
        }
        Ok(Some(account)) => account,
        Ok(None) => {
            state
                .mail_rebuilds
                .lock()
                .expect("mail rebuild lock poisoned")
                .remove(&account.id);
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(anyhow::anyhow!("Account not found"));
        }
    };
    // The durable job is the source of truth while holding the account lock.
    // A queued worker may have captured an older `false` before an update
    // atomically replaced its job with `true`; never downgrade that reset.
    let durable_reset = match state.store.mail_rebuild_jobs().await {
        Ok(jobs) => jobs
            .into_iter()
            .find(|job| job.account_id == account.id)
            .map(|job| job.reset_before_sync),
        Err(error) => {
            state.mail_rebuild_cancellations.clear(account.id);
            release_mail_rebuild(&state, account.id);
            return Err(error);
        }
    };
    let reset_before_sync = effective_rebuild_reset(reset_before_sync, durable_reset);
    let result = run_mail_rebuild_locked(
        app,
        state.clone(),
        current_account,
        reset_before_sync,
        cancel_receiver,
    )
    .await;
    // This is deliberately outside the worker body: database failures before
    // the first checkpoint must not leave a queued cancellation or a stale
    // duplicate-run reservation behind.
    state.mail_rebuild_cancellations.clear(account.id);
    release_mail_rebuild(&state, account.id);
    result
}

async fn run_mail_rebuild_locked(
    app: tauri::AppHandle,
    state: Arc<AppState>,
    account: Account,
    reset_before_sync: bool,
    mut cancel_receiver: watch::Receiver<MailRebuildCancellationDisposition>,
) -> anyhow::Result<SyncResult> {
    let initial = MailRebuildJob {
        account_id: account.id,
        phase: "connecting".to_owned(),
        completed: 0,
        total: None,
        reset_before_sync,
    };
    let mut stopped_realtime = false;
    let result = async {
        if state
            .mail_rebuild_cancellations
            .disposition(&cancel_receiver)
            != MailRebuildCancellationDisposition::None
        {
            return Err(anyhow::anyhow!("mail rebuild cancelled"));
        }
        state.realtime.stop_account(account.id).await;
        stopped_realtime = true;
        state.store.save_mail_rebuild_job(&initial).await?;
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .insert(account.id, initial.clone().into());

        let service = MailService::new(state.store.clone());
        let progress_app = app.clone();
        let progress_state = state.clone();
        let account_id = account.id;
        let rebuild = async {
            if reset_before_sync {
                service
                    .rebuild_all_with_progress(&account, 250, move |progress| {
                        publish_mail_rebuild_progress(
                            &progress_app,
                            &progress_state,
                            account_id,
                            progress,
                        );
                    })
                    .await
            } else {
                service
                    .resume_rebuild_all_with_progress(&account, 250, move |progress| {
                        publish_mail_rebuild_progress(
                            &progress_app,
                            &progress_state,
                            account_id,
                            progress,
                        );
                    })
                    .await
            }
        };
        tokio::pin!(rebuild);
        tokio::select! {
            result = &mut rebuild => result,
            changed = cancel_receiver.changed() => {
                match changed {
                    Ok(()) if state.mail_rebuild_cancellations.disposition(&cancel_receiver)
                        != MailRebuildCancellationDisposition::None =>
                    {
                        Err(anyhow::anyhow!("mail rebuild cancelled"))
                    }
                    _ => rebuild.await,
                }
            }
        }
    }
    .await;
    let disposition = state
        .mail_rebuild_cancellations
        .disposition(&cancel_receiver);

    let mut result = result;
    let retains_durable_job = result.as_ref().err().is_some_and(|failure| {
        should_retain_mail_rebuild_job(failure, disposition, reset_before_sync)
    });
    if result.is_ok() {
        if let Err(error) = state.store.delete_mail_rebuild_job(account.id).await {
            result = Err(error);
        }
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
        if result.is_ok() {
            let _ = app.emit(
                "mail-index-rebuilt",
                serde_json::json!({ "accountId": account.id }),
            );
            kick_classification(state.clone());
        }
    } else if retains_durable_job {
        // Leave the persisted job intact so application startup can resume
        // this interrupted rebuild. Save the last published checkpoint before
        // clearing the in-memory entry, so a restart resumes with truthful
        // progress instead of the initial connecting state.
        let latest = state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
        if let Some(latest) = latest {
            if let Err(error) = state
                .store
                .save_mail_rebuild_job(&MailRebuildJob {
                    account_id: latest.account_id,
                    phase: latest.phase,
                    completed: latest.completed,
                    total: latest.total,
                    reset_before_sync: latest.reset_before_sync,
                })
                .await
            {
                tracing::warn!(
                    account_id = %account.id,
                    error = %error,
                    "could not persist failed mail rebuild checkpoint"
                );
            }
        }
    } else {
        if let Err(error) = state.store.delete_mail_rebuild_job(account.id).await {
            tracing::warn!(account_id = %account.id, error = %error, "could not delete abandoned mail rebuild job");
        }
        state
            .mail_rebuilds
            .lock()
            .expect("mail rebuild lock poisoned")
            .remove(&account.id);
    }
    let outcome = if result.is_ok() {
        "completed"
    } else if disposition != MailRebuildCancellationDisposition::None {
        "cancelled"
    } else {
        "failed"
    };
    let _ = app.emit(
        "mail-rebuild-finished",
        serde_json::json!({ "accountId": account.id, "outcome": outcome }),
    );
    // Do not restart realtime into an old catalogue while a replacement reset
    // remains reserved. The resumed rebuild owns restarting it on success.
    if stopped_realtime && !(retains_durable_job && reset_before_sync) {
        if let Err(error) = restart_realtime_if_current(app, &state, account.id).await {
            if result.is_ok() {
                return Err(error);
            }
            tracing::warn!(account_id = %account.id, error = %error, "could not restart realtime after mail rebuild");
        }
    }
    result
}

#[tauri::command]
async fn mail_rebuild_status(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<MailRebuildProgress>, String> {
    Ok(state
        .mail_rebuilds
        .lock()
        .map_err(error)?
        .values()
        .cloned()
        .collect())
}

#[tauri::command]
async fn sync_account(
    app: tauri::AppHandle,
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    limit: Option<u32>,
    full: Option<bool>,
    on_progress: Channel<SyncProgress>,
) -> Result<SyncResult, String> {
    let result = if full.unwrap_or(false) {
        if !reserve_mail_rebuild(state.inner(), account_id) {
            return Err("A mail re-index is already running for this account".to_owned());
        }
        let account = state.store.account(account_id).await.map_err(error);
        let account = match account
            .and_then(|account| account.ok_or_else(|| "Account not found".to_owned()))
        {
            Ok(account) => account,
            Err(error) => {
                release_mail_rebuild(state.inner(), account_id);
                return Err(error);
            }
        };
        let reset_before_sync = match state.mail_rebuilds.lock().map_err(error) {
            Ok(rebuilds) => rebuilds
                .get(&account_id)
                .map(|job| job.reset_before_sync)
                .unwrap_or(true),
            Err(lock_error) => {
                release_mail_rebuild(state.inner(), account_id);
                return Err(lock_error);
            }
        };
        if let Err(schedule_error) =
            schedule_mail_rebuild(state.inner(), account_id, reset_before_sync).await
        {
            release_mail_rebuild(state.inner(), account_id);
            return Err(error(schedule_error));
        }
        let result = run_mail_rebuild(
            app.clone(),
            state.inner().clone(),
            account.clone(),
            reset_before_sync,
        )
        .await;
        if result.is_ok() {
            let _ = on_progress.send(SyncProgress {
                phase: "complete",
                completed: 1,
                total: Some(1),
            });
        }
        (result, account)
    } else {
        run_incremental_sync_account(
            state.inner(),
            account_id,
            limit.unwrap_or(50),
            |progress| {
                let _ = on_progress.send(progress);
            },
            || restart_realtime_if_current(app.clone(), state.inner(), account_id),
        )
        .await?
    };
    let (result, _) = result;
    let synced = result.map_err(error)?;
    kick_classification(state.inner().clone());
    Ok(synced)
}

async fn run_incremental_sync_account<Progress, Restart, RestartFuture>(
    state: &Arc<AppState>,
    account_id: Uuid,
    limit: u32,
    on_progress: Progress,
    restart: Restart,
) -> Result<(anyhow::Result<SyncResult>, Account), String>
where
    Progress: FnMut(SyncProgress),
    Restart: FnOnce() -> RestartFuture,
    RestartFuture: Future<Output = anyhow::Result<()>>,
{
    let _operation = state.account_operations.acquire(account_id).await;
    let account = state
        .store
        .account(account_id)
        .await
        .map_err(error)?
        .ok_or_else(|| "Account not found".to_owned())?;
    state.realtime.stop_account(account_id).await;
    let service = MailService::new(state.store.clone());
    let result = service
        .refresh_inbox_with_progress(&account, limit, on_progress)
        .await;
    let result = complete_manual_sync_attempt(result, restart().await, account_id);
    Ok((result, account))
}

#[derive(Deserialize)]
struct SendCommandDraft {
    #[serde(flatten)]
    message: ComposeMessage,
    #[serde(default)]
    send_attempt_id: Option<Uuid>,
}

// Retained as an isolated journal regression harness for existing tests. The
// production command below uses the recovery payload transaction instead.
#[cfg(test)]
async fn submit_send_attempt(
    store: &Store,
    draft: &ComposeMessage,
    attempt_id: Option<Uuid>,
    submit: impl Future<Output = anyhow::Result<String>>,
) -> Result<String, String> {
    use dakia_core::storage::{SendAttemptClaim, SendAttemptStatus};
    use dakia_core::SendFailureKind;
    use sha2::{Digest, Sha256};
    let Some(attempt_id) = attempt_id else {
        return submit.await.map_err(send_command_error);
    };
    let payload_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(draft).map_err(|error| send_command_error(error.into()))?
        )
    );
    let failure_for = |kind| {
        send_command_error(anyhow::Error::new(dakia_core::SendFailure {
            kind,
            message: String::new(),
        }))
    };
    match store
        .claim_send_attempt(draft.account_id, attempt_id, &payload_hash)
        .await
        .map_err(send_command_error)?
    {
        SendAttemptClaim::Claimed => {}
        SendAttemptClaim::Existing(SendAttemptStatus::Submitted) => {
            return Ok("Message already submitted".into())
        }
        SendAttemptClaim::Existing(SendAttemptStatus::SentCopyFailed) => {
            return Err(failure_for(SendFailureKind::SentCopyFailed))
        }
        SendAttemptClaim::Existing(_) | SendAttemptClaim::Mismatch => {
            return Err(failure_for(SendFailureKind::DeliveryUncertain))
        }
    }
    match submit.await {
        Ok(response) => {
            store
                .complete_send_attempt(
                    draft.account_id,
                    attempt_id,
                    &payload_hash,
                    SendAttemptStatus::Submitted,
                )
                .await
                .map_err(send_command_error)?;
            Ok(response)
        }
        Err(failure) => {
            let kind = failure
                .downcast_ref::<dakia_core::SendFailure>()
                .map(|failure| failure.kind)
                .unwrap_or(SendFailureKind::DeliveryUncertain);
            match kind {
                SendFailureKind::NotSubmitted => store
                    .release_send_attempt(draft.account_id, attempt_id, &payload_hash)
                    .await
                    .map_err(send_command_error)?,
                SendFailureKind::DeliveryUncertain | SendFailureKind::SentCopyFailed => {
                    let status = if kind == SendFailureKind::SentCopyFailed {
                        SendAttemptStatus::SentCopyFailed
                    } else {
                        SendAttemptStatus::DeliveryUncertain
                    };
                    store
                        .complete_send_attempt(draft.account_id, attempt_id, &payload_hash, status)
                        .await
                        .map_err(send_command_error)?;
                }
            }
            Err(send_command_error(failure))
        }
    }
}

/// Sends a draft after its canonical payload and outcome journal have been
/// made durable. The claim is intentionally created before the account lock:
/// a process loss while a send waits in that queue remains visible as Pending
/// instead of silently disappearing.
async fn submit_recoverable_send_attempt<R: tauri::Runtime>(
    state: &AppState,
    runtime: Arc<SendRecoveryRuntime>,
    app: tauri::AppHandle<R>,
    draft: &ComposeMessage,
    attempt_id: Uuid,
) -> Result<String, String> {
    use dakia_core::storage::{SendAttemptStatus, SendRecoveryClaim, SendRecoveryStatus};
    use dakia_core::SendFailureKind;
    let failure_for = |kind| {
        send_command_error(anyhow::Error::new(dakia_core::SendFailure {
            kind,
            message: String::new(),
        }))
    };
    // Persist after structural/MIME validation but before enabled-account and
    // account-lock checks. A disabled account is a definite no-SMTP outcome,
    // yet its draft must still survive a closed Compose window as NotSubmitted.
    let account = state
        .store
        .account(draft.account_id)
        .await
        .map_err(|_| failure_for(SendFailureKind::NotSubmitted))?
        .ok_or_else(|| failure_for(SendFailureKind::NotSubmitted))?;
    dakia_core::mail::validate_compose_message_for_account(&account, draft)
        .map_err(send_command_error)?;
    let existing_recovery = state
        .store
        .load_send_recovery(draft.account_id, attempt_id)
        .await
        .map_err(send_command_error)?
        .is_some();
    let active = runtime.reserve(draft.account_id, attempt_id, app, existing_recovery)?;
    match state
        .store
        .prepare_send_recovery(draft.account_id, attempt_id, draft)
        .await
        .map_err(send_command_error)?
    {
        SendRecoveryClaim::Claimed => active.mark_persisted()?,
        SendRecoveryClaim::AlreadySubmitted => {
            return Ok("Message already submitted".into());
        }
        SendRecoveryClaim::Existing(SendRecoveryStatus::SentCopyFailed) => {
            return Err(failure_for(SendFailureKind::SentCopyFailed));
        }
        SendRecoveryClaim::Existing(_) | SendRecoveryClaim::Mismatch => {
            return Err(failure_for(SendFailureKind::DeliveryUncertain));
        }
    }
    let _operation = state.account_operations.acquire(draft.account_id).await;
    let account = match enabled_account_for_operation(state, draft.account_id).await {
        Ok(account) => account,
        Err(_) => {
            state
                .store
                .release_send_recovery(draft.account_id, attempt_id)
                .await
                .map_err(|_| failure_for(SendFailureKind::DeliveryUncertain))?;
            return Err(failure_for(SendFailureKind::NotSubmitted));
        }
    };
    if dakia_core::mail::validate_compose_message_for_account(&account, draft).is_err() {
        state
            .store
            .release_send_recovery(draft.account_id, attempt_id)
            .await
            .map_err(|_| failure_for(SendFailureKind::DeliveryUncertain))?;
        return Err(failure_for(SendFailureKind::NotSubmitted));
    }
    let service = MailService::new(state.store.clone());
    let result = match service.send(&account, draft).await {
        Ok(response) => {
            state
                .store
                .finish_send_recovery(draft.account_id, attempt_id, SendAttemptStatus::Submitted)
                .await
                .map_err(|_| failure_for(SendFailureKind::DeliveryUncertain))?;
            Ok(response)
        }
        Err(failure) => {
            let kind = failure
                .downcast_ref::<dakia_core::SendFailure>()
                .map(|failure| failure.kind)
                .unwrap_or(SendFailureKind::DeliveryUncertain);
            match kind {
                SendFailureKind::NotSubmitted => {
                    state
                        .store
                        .release_send_recovery(draft.account_id, attempt_id)
                        .await
                        .map_err(|_| failure_for(SendFailureKind::DeliveryUncertain))?;
                }
                SendFailureKind::DeliveryUncertain | SendFailureKind::SentCopyFailed => {
                    let status = if kind == SendFailureKind::SentCopyFailed {
                        SendAttemptStatus::SentCopyFailed
                    } else {
                        SendAttemptStatus::DeliveryUncertain
                    };
                    state
                        .store
                        .finish_send_recovery(draft.account_id, attempt_id, status)
                        .await
                        .map_err(|_| failure_for(SendFailureKind::DeliveryUncertain))?;
                }
            }
            Err(send_command_error(failure))
        }
    };
    // Dropping the guard removes the active-row filter before notifying the
    // main window, so recovery listing observes the terminal durable state.
    drop(active);
    result
}

#[tauri::command]
async fn send_message<R: tauri::Runtime>(
    state: State<'_, Arc<AppState>>,
    runtime: State<'_, Arc<SendRecoveryRuntime>>,
    app: tauri::AppHandle<R>,
    draft: SendCommandDraft,
) -> Result<String, String> {
    if let Some(attempt_id) = draft.send_attempt_id {
        return submit_recoverable_send_attempt(
            state.inner(),
            Arc::clone(runtime.inner()),
            app,
            &draft.message,
            attempt_id,
        )
        .await;
    }
    // Compatibility for an already-issued older Compose window. New Compose
    // always provides an ID and therefore always has durable recovery.
    let _operation = state
        .account_operations
        .acquire(draft.message.account_id)
        .await;
    let account = enabled_account_for_operation(state.inner(), draft.message.account_id)
        .await
        .map_err(|_| {
            send_command_error(anyhow::Error::new(dakia_core::SendFailure {
                kind: dakia_core::SendFailureKind::NotSubmitted,
                message: "Account is unavailable".into(),
            }))
        })?;
    MailService::new(state.store.clone())
        .send(&account, &draft.message)
        .await
        .map_err(send_command_error)
}

#[tauri::command]
async fn apply_mailbox_action(
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    mailbox: String,
    uid: u32,
    action: MailboxAction,
    message_id: Option<String>,
    expected_uid_validity: Option<i64>,
) -> Result<(), String> {
    apply_mailbox_action_inner(
        state.inner(),
        account_id,
        mailbox,
        uid,
        action,
        message_id,
        expected_uid_validity,
    )
    .await
}

async fn apply_mailbox_action_inner(
    state: &Arc<AppState>,
    account_id: Uuid,
    mailbox: String,
    uid: u32,
    action: MailboxAction,
    message_id: Option<String>,
    expected_uid_validity: Option<i64>,
) -> Result<(), String> {
    let _operation = state.account_operations.acquire(account_id).await;
    let account = enabled_account_for_operation(state, account_id).await?;
    let target = require_current_message_target(
        &state.store,
        account_id,
        &mailbox,
        uid,
        message_id.as_deref(),
        expected_uid_validity,
    )
    .await?;
    let receipt = MailService::new(state.store.clone())
        .apply_action_at_epoch_with_receipt(
            &account,
            &mailbox,
            uid,
            action,
            expected_uid_validity.expect("target validated"),
        )
        .await
        .map_err(error)?;
    let publication = state
        .store
        .move_message_at_epoch(
            &target,
            mailbox_action_destination(action).unwrap_or_default(),
            &receipt,
        )
        .await
        .map_err(error)?;
    if !publication.source_published {
        return Err(message_identity_changed_error());
    }
    if mailbox_action_destination(action).is_some() && !publication.destination_mapped {
        // Without a verified COPYUID destination receipt, the source is still
        // safely tombstoned but the destination must be projected by sync.
        let store = state.store.clone();
        let account = account.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(sync_error) = MailService::new(store).sync_all(&account, 500).await {
                tracing::warn!(account_id = %account.id, error = %sync_error, "could not reconcile mailbox action destination");
            }
        });
    }
    Ok(())
}

async fn require_current_message_target(
    store: &Store,
    account_id: Uuid,
    mailbox: &str,
    uid: u32,
    message_id: Option<&str>,
    expected_uid_validity: Option<i64>,
) -> Result<MailSummary, String> {
    let unavailable =
        || "This message changed. Refresh the mailbox before trying again.".to_owned();
    let id = message_id
        .filter(|value| !value.is_empty())
        .ok_or_else(unavailable)?;
    let expected = expected_uid_validity
        .filter(|value| *value > 0)
        .ok_or_else(unavailable)?;
    let message = store
        .message(id)
        .await
        .map_err(error)?
        .ok_or_else(unavailable)?;
    if message.account_id != account_id.to_string()
        || message.mailbox != mailbox
        || message.uid != i64::from(uid)
        || message.mailbox_uid_validity != Some(expected)
    {
        return Err(unavailable());
    }
    Ok(message)
}

#[cfg(test)]
async fn require_permanent_delete_locator(
    store: &Store,
    account_id: Uuid,
    mailbox: &str,
    uid: u32,
    action: MailboxAction,
) -> Result<(), String> {
    if !matches!(action, MailboxAction::Delete) {
        return Ok(());
    }
    store
        .message_by_locator(account_id, mailbox, uid)
        .await
        .map_err(error)?
        .ok_or_else(|| "Message is no longer available in this mailbox".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod permanent_delete_command_tests {
    use super::*;
    use chrono::Utc;

    fn message(account_id: Uuid, mailbox: &str, uid: u32) -> MailSummary {
        MailSummary {
            mailbox_uid_validity: None,
            id: format!("{account_id}:{mailbox}:{uid}"),
            account_id: account_id.to_string(),
            mailbox: mailbox.into(),
            uid: i64::from(uid),
            message_id: Some(format!("<{uid}@example.test>")),
            gmail_message_id: None,
            in_reply_to: None,
            reference_ids: None,
            thread_id: format!("thread-{uid}"),
            subject: "Permanent delete locator".into(),
            from_name: None,
            from_address: "sender@example.test".into(),
            to_addresses: "reader@example.test".into(),
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
            attachments: vec![],
        }
    }

    #[tokio::test]
    async fn permanent_delete_requires_the_exact_local_account_mailbox_and_uid() {
        let store = Store::in_memory().await.expect("in-memory store");
        let account_id = Uuid::new_v4();
        let other_account_id = Uuid::new_v4();
        store
            .upsert_messages(&[message(account_id, "INBOX", 42)])
            .await
            .expect("save message");

        require_permanent_delete_locator(&store, account_id, "INBOX", 42, MailboxAction::Delete)
            .await
            .expect("exact locator is accepted");
        for (candidate_account, candidate_mailbox, candidate_uid) in [
            (other_account_id, "INBOX", 42),
            (account_id, "Archive", 42),
            (account_id, "INBOX", 41),
        ] {
            assert!(
                require_permanent_delete_locator(
                    &store,
                    candidate_account,
                    candidate_mailbox,
                    candidate_uid,
                    MailboxAction::Delete,
                )
                .await
                .is_err(),
                "a crossed or absent locator must fail before IMAP"
            );
        }
        require_permanent_delete_locator(
            &store,
            other_account_id,
            "INBOX",
            42,
            MailboxAction::Trash,
        )
        .await
        .expect("ordinary Trash behavior remains unchanged");
    }
}

#[tauri::command]
async fn unsubscribe_message<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, Arc<AppState>>,
    message_id: String,
    expected_uid_validity: Option<i64>,
) -> Result<UnsubscribeResult, String> {
    // The unsubscribe URL is untrusted message data. Bind it to the selected
    // IMAP namespace before it can open a browser, send a mailto message, or
    // issue a one-click POST.
    let initial =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    let account_id = Uuid::parse_str(&initial.account_id).map_err(error)?;
    let _operation = state.account_operations.acquire(account_id).await;
    let message =
        current_message_at_epoch(&state.store, &message_id, expected_uid_validity).await?;
    if !same_message_epoch_identity(&initial, &message) {
        return Err(message_identity_changed_error());
    }
    let mut cleanup_target = sender_cleanup_target(&message, account_id);
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    let service = MailService::new(state.store.clone());
    let outcome = match unsubscribe_with_command_transport(&service, &message).await {
        Ok(outcome) => outcome,
        // Older catalogue rows may contain an action selected by a previous
        // parser version. Refresh only malformed, side-effect-free web/mailto
        // metadata so a later valid fallback in the header can be selected.
        // Never retry a one-click POST: its failure may be ambiguous.
        Err(_) if message.unsubscribe_kind.as_deref() != Some("one_click") => {
            let fetched = service
                .fetch_message(&account, &message.mailbox, message.uid as u32)
                .await
                .map_err(error)?;
            if !same_message_epoch_identity(&message, &fetched) {
                return Err(message_identity_changed_error());
            }
            if !state
                .store
                .publish_message_content_at_epoch(&message, &fetched)
                .await
                .map_err(error)?
            {
                return Err(message_identity_changed_error());
            }
            let refreshed = confirm_message_epoch(&state.store, &message).await?;
            cleanup_target = sender_cleanup_target(&refreshed, account_id);
            unsubscribe_with_command_transport(&service, &refreshed)
                .await
                .map_err(error)?
        }
        Err(failure) => return Err(error(failure)),
    };
    match outcome {
        UnsubscribeOutcome::Completed => Ok(UnsubscribeResult::Completed { cleanup_target }),
        UnsubscribeOutcome::Web(url) => {
            confirm_message_epoch(&state.store, &message).await?;
            open_external_url(app, url)?;
            Ok(UnsubscribeResult::OpenedWeb { cleanup_target })
        }
        UnsubscribeOutcome::Mailto { to, subject, body } => {
            confirm_message_epoch(&state.store, &message).await?;
            let draft = unsubscribe_email(account_id, to, subject, body)?;
            MailService::new(state.store.clone())
                .send(&account, &draft)
                .await
                .map_err(error)?;
            Ok(UnsubscribeResult::Completed { cleanup_target })
        }
    }
}

#[tauri::command]
async fn trash_messages_from_sender(
    state: State<'_, Arc<AppState>>,
    account_id: Uuid,
    sender_address: String,
) -> Result<SenderTrashResult, String> {
    let _operation = state.account_operations.acquire(account_id).await;
    let account = enabled_account_for_operation(state.inner(), account_id).await?;
    MailService::new(state.store.clone())
        .trash_messages_from_sender(&account, &sender_address)
        .await
        .map_err(error)
}

#[tauri::command]
async fn ai_summarize(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<String, String> {
    require_ai_features_enabled()?;
    let targets = required_ai_message_targets(&input)?;
    let messages = hydrate_messages(state.inner(), &targets).await?;
    confirm_ai_message_targets(&state.store, &targets).await?;
    let service = ai_service(&state.store, input).await?;
    confirm_ai_message_targets(&state.store, &targets).await?;
    service.summarize(&messages).await.map_err(error)
}

#[tauri::command]
async fn ai_draft(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<String, String> {
    require_ai_features_enabled()?;
    ai_draft_inner(state.inner(), input).await
}

async fn ai_draft_inner(state: &Arc<AppState>, input: AiInput) -> Result<String, String> {
    require_ai_features_enabled()?;
    let targets = required_ai_message_targets(&input)?;
    let messages = hydrate_messages(state, &targets).await?;
    confirm_ai_message_targets(&state.store, &targets).await?;
    let instruction = input.instruction.clone().unwrap_or_default();
    let service = ai_service(&state.store, input).await?;
    confirm_ai_message_targets(&state.store, &targets).await?;
    service.draft(&instruction, &messages).await.map_err(error)
}

fn required_ai_message_targets(input: &AiInput) -> Result<Vec<AiMessageTarget>, String> {
    if input.message_ids.is_empty()
        || input.message_ids.len() != input.message_targets.len()
        || input
            .message_ids
            .iter()
            .zip(&input.message_targets)
            .any(|(id, target)| {
                id.is_empty() || id != &target.message_id || target.expected_uid_validity.is_none()
            })
    {
        return Err(message_identity_changed_error());
    }
    let mut ids = HashSet::new();
    if input
        .message_targets
        .iter()
        .any(|target| !ids.insert(target.message_id.as_str()))
    {
        return Err(message_identity_changed_error());
    }
    Ok(input.message_targets.clone())
}

async fn confirm_ai_message_targets(
    store: &Store,
    targets: &[AiMessageTarget],
) -> Result<(), String> {
    for target in targets {
        current_message_at_epoch(store, &target.message_id, target.expected_uid_validity).await?;
    }
    Ok(())
}

async fn hydrate_messages(
    state: &Arc<AppState>,
    targets: &[AiMessageTarget],
) -> Result<Vec<MailSummary>, String> {
    confirm_ai_message_targets(&state.store, targets).await?;
    let hydration_state = state.clone();
    run_bounded_ordered(
        targets.to_vec(),
        MESSAGE_HYDRATION_CONCURRENCY,
        state.remote_operation_slots.clone(),
        move |target| {
            let state = hydration_state.clone();
            async move {
                hydrated_message(&state, &target.message_id, target.expected_uid_validity).await
            }
        },
    )
    .await
    .into_iter()
    .collect()
}

#[tauri::command]
async fn ai_available(state: State<'_, Arc<AppState>>, input: AiInput) -> Result<bool, String> {
    require_ai_features_enabled()?;
    Ok(ai_service(&state.store, input).await?.is_available().await)
}

#[tauri::command]
async fn send_desktop_notification<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    notification: DesktopNotification,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let availability = app
            .try_state::<NativeNotificationAvailability>()
            .ok_or_else(|| NATIVE_NOTIFICATION_UNAVAILABLE_ERROR.to_owned())?;
        with_native_notification_identity(&availability, || {
            let mut builder = notify_rust::Notification::new();
            builder
                .summary(&notification.title)
                .body(&notification.body)
                .auto_icon();
            if let Some(sound) = &notification.sound {
                builder.sound_name(sound);
            }
            let handle = builder.show().map_err(error)?;
            let action_app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                handle.wait_for_action(move |action| {
                    if action == "__closed" {
                        return;
                    }
                    if !notification_has_reader_target(&notification) {
                        if let Some(window) = action_app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    let _ = action_app.emit("notification-action", notification);
                });
            });
            Ok(())
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        let _ = notification;
        Err("Native desktop notifications are only used on macOS".to_owned())
    }
}

async fn ai_service(store: &Store, input: AiInput) -> Result<AiService, String> {
    let provider = match input.provider.as_str() {
        "ollama" => AiProvider::Ollama {
            base_url: Url::parse(
                input
                    .base_url
                    .as_deref()
                    .unwrap_or("http://127.0.0.1:11434/"),
            )
            .map_err(error)?,
            model: input.model,
        },
        "openai" => AiProvider::OpenAiCompatible {
            base_url: Url::parse(
                input
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1/"),
            )
            .map_err(error)?,
            model: input.model,
        },
        "local" => AiProvider::LocalCommand {
            executable: input
                .executable
                .ok_or_else(|| "Local AI executable is required".to_owned())?,
            model_path: input
                .model_path
                .ok_or_else(|| "Local model path is required".to_owned())?,
            extra_args: Vec::new(),
        },
        _ => return Err("Unknown AI provider".into()),
    };
    let api_key = match input.api_key.filter(|value| !value.is_empty()) {
        Some(value) => Some(value),
        None => store
            .secret("dev.dakia.mail:ai:api-key")
            .await
            .map_err(error)?,
    }
    .map(SecretString::from);
    Ok(AiService::new(AiConfig { provider, api_key }))
}

#[tauri::command]
async fn set_ai_api_key(state: State<'_, Arc<AppState>>, api_key: String) -> Result<(), String> {
    require_ai_features_enabled()?;
    if api_key.is_empty() {
        state
            .store
            .delete_secret("dev.dakia.mail:ai:api-key")
            .await
            .map_err(error)?;
    } else {
        state
            .store
            .set_secret("dev.dakia.mail:ai:api-key", &api_key)
            .await
            .map_err(error)?;
    }
    Ok(())
}

#[tauri::command]
fn translation_models(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<TranslationModelStatus>, String> {
    translation::statuses(&state.data_dir)
}

#[tauri::command]
fn translation_model_files(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<TranslationModelFiles, String> {
    translation::files(&state.data_dir, &source)
}

#[tauri::command]
fn translation_detect_language(text: String) -> TranslationLanguageDetection {
    translation::detect_language(&text)
}

#[tauri::command]
async fn translation_install_model(
    state: State<'_, Arc<AppState>>,
    source: String,
    on_progress: Channel<TranslationDownloadProgress>,
) -> Result<TranslationModelFiles, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    state
        .translation_downloads
        .lock()
        .map_err(error)?
        .insert(source.clone(), cancelled.clone());
    let result = translation::install(&state.data_dir, &source, on_progress, cancelled).await;
    state
        .translation_downloads
        .lock()
        .map_err(error)?
        .remove(&source);
    result
}

#[tauri::command]
fn translation_cancel_install(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<(), String> {
    if let Some(cancelled) = state
        .translation_downloads
        .lock()
        .map_err(error)?
        .get(&source)
    {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
async fn translation_remove_model(
    state: State<'_, Arc<AppState>>,
    source: String,
) -> Result<(), String> {
    translation::remove(&state.data_dir, &source).await
}

#[tauri::command]
fn quit_startup_recovery<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    window: tauri::WebviewWindow<R>,
    exit_status: State<'_, StartupFailureExitStatus>,
) -> Result<(), String> {
    if !exit_status.may_exit_from_fatal_recovery_window(window.label()) {
        return Err("Fatal recovery exit is unavailable".into());
    }

    exit_status.mark_requested();
    app.exit(startup_storage_failure_exit_code());
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let dropped_file_receipts = Arc::new(DroppedFileReceiptStore::default());
    let event_receipts = dropped_file_receipts.clone();
    let window_receipts = dropped_file_receipts.clone();
    let send_recovery_runtime = Arc::new(SendRecoveryRuntime::default());
    let window_send_recoveries = send_recovery_runtime.clone();
    let startup_failure_exit_status = StartupFailureExitStatus::default();
    let setup_startup_failure_exit_status = startup_failure_exit_status.clone();
    let menu_startup_failure_exit_status = startup_failure_exit_status.clone();
    #[cfg(target_os = "macos")]
    let native_notification_availability = NativeNotificationAvailability::default();
    #[cfg(target_os = "macos")]
    let setup_native_notification_availability = native_notification_availability.clone();
    let builder = tauri::Builder::default()
        .manage(startup_failure_exit_status.clone())
        .manage(dropped_file_receipts)
        .manage(send_recovery_runtime);
    #[cfg(target_os = "macos")]
    let builder = builder.manage(native_notification_availability);
    let app = builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_menu_event(move |app, event| {
            let action = event.id().as_ref();
            if let Some(exit_code) = menu_startup_failure_exit_status.app_menu_quit_exit_code(action) {
                app.exit(exit_code);
                return;
            }
            if let Some(encoded_address) = action.strip_prefix("copy-email-address:") {
                match decode_context_menu_email_address(encoded_address)
                    .and_then(|address| app.clipboard().write_text(address).map_err(error))
                {
                    Ok(()) => {}
                    Err(copy_error) => {
                        tracing::warn!(error = %copy_error, "could not copy email address");
                        let target = app
                            .webview_windows()
                            .into_values()
                            .find(|window| window.is_focused().unwrap_or(false))
                            .or_else(|| app.get_webview_window("main"));
                        if let Some(window) = target {
                            let _ = window.emit("menu-action", "copy-email-address-failed");
                        }
                    }
                }
                return;
            }
            let focused = menu_action_targets_focused_window(action).then(|| {
                app.webview_windows()
                    .into_values()
                    .find(|window| window.is_focused().unwrap_or(false))
            });
            if let Some(Some(window)) = focused {
                let _ = window.emit("menu-action", action);
            } else if let Some(main) = app.get_webview_window("main") {
                let _ = main.emit("menu-action", action);
            }
        })
        .on_window_event(move |window, event| match event {
            WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                match event_receipts.issue(window.label(), paths.clone()) {
                    Ok(receipt) => {
                        let expiry_receipts = event_receipts.clone();
                        let expiry_receipt = receipt.clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(DROPPED_FILE_RECEIPT_TTL).await;
                            expiry_receipts.expire_at(&expiry_receipt, Instant::now());
                        });
                        if let Err(emit_error) = window.emit(DROPPED_FILE_RECEIPT_EVENT, receipt) {
                            tracing::warn!(
                                error = %emit_error,
                                window = window.label(),
                                "could not deliver dropped-file receipt"
                            );
                        }
                    }
                    Err(drop_error) => {
                        if let Err(emit_error) = window.emit(DROPPED_FILE_ERROR_EVENT, drop_error) {
                            tracing::warn!(
                                error = %emit_error,
                                window = window.label(),
                                "could not deliver dropped-file error"
                            );
                        }
                    }
                }
            }
            WindowEvent::Destroyed => {
                window_receipts.revoke_window(window.label());
                window_send_recoveries.revoke_window(window.label());
            }
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                // notify-rust uses process-global state for the application identity. It
                // rejects subsequent calls, so initialize it once before either the
                // frontend notification plugin or realtime mail notifications run.
                let is_dev = tauri::is_dev();
                let application = if is_dev {
                    "com.apple.Terminal"
                } else {
                    app.config().identifier.as_str()
                };
                initialize_native_notification_application(
                    &setup_native_notification_availability,
                    application,
                    move |application| {
                        let expected_app = if is_dev {
                            None
                        } else {
                            Some(current_macos_application_bundle()?)
                        };
                        preflight_native_notification_application(
                            application,
                            expected_app.as_deref(),
                            resolve_launch_services_application_urls,
                        )
                    },
                    |application| notify_rust::set_application(application).map_err(error),
                    |notification_error| {
                        tracing::warn!(
                            application,
                            error = %notification_error,
                            "desktop notifications are unavailable because the application identity could not be verified"
                        );
                    },
                );
            }
            install_app_menu(app).map_err(|error| anyhow::anyhow!("app menu: {error}"))?;
            #[cfg(feature = "native-mail-safety-fixture")]
            let native_mail_safety_fixture = NativeMailSafetyFixture::from_environment()?;
            #[cfg(feature = "native-mail-safety-fixture")]
            let native_fixture_data_dir = native_mail_safety_fixture
                .as_ref()
                .map(|fixture| fixture.data_dir.clone());
            #[cfg(not(feature = "native-mail-safety-fixture"))]
            let native_fixture_data_dir: Option<PathBuf> = None;
            let release_smoke_test = std::env::var_os("DAKIA_RELEASE_SMOKE_TEST").as_deref()
                == Some(std::ffi::OsStr::new("1"));
            let data_dir = if release_smoke_test {
                std::env::var_os("DAKIA_RELEASE_SMOKE_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "DAKIA_RELEASE_SMOKE_DATA_DIR is required for release smoke tests"
                        )
                    })?
            } else if let Some(data_dir) = native_fixture_data_dir {
                data_dir
            } else {
                app.path()
                    .app_local_data_dir()
                    .map_err(|error| anyhow::anyhow!("local data directory: {error}"))?
            };
            let resource_dir = match app.path().resource_dir() {
                Ok(path) => path,
                Err(_) => std::env::current_exe()?
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("development executable has no parent"))?
                    .to_path_buf(),
            };
            let classifier_dir = resource_dir.join("resources/email-classifier-v2");
            let store = match tauri::async_runtime::block_on(Store::open(data_dir.join("dakia.db")))
            {
                Ok(store) => store,
                Err(error) => {
                    setup_startup_failure_exit_status.activate_fatal_recovery();
                    let exit_handle = app.handle().clone();
                    let callback_exit_status = setup_startup_failure_exit_status.clone();
                    let mut presenter = TauriStartupFailurePresenter::new(app);
                    present_startup_storage_failure(&mut presenter, &error, move || {
                        callback_exit_status.mark_requested();
                        exit_handle.exit(startup_storage_failure_exit_code());
                    });
                    return Ok(());
                }
            };
            let state = tauri::async_runtime::block_on(async {
                #[cfg(feature = "native-mail-safety-fixture")]
                if let Some(fixture) = &native_mail_safety_fixture {
                    fixture.seed(&store).await?;
                }
                let classifier = LocalEmailClassifier::from_dir(&classifier_dir)?;
                let mail_rebuilds = store
                    .mail_rebuild_jobs()
                    .await?
                    .into_iter()
                    .map(|job| (job.account_id, job.into()))
                    .collect();
                anyhow::Ok(Arc::new(AppState {
                    realtime: RealtimeSyncManager::new(store.clone()),
                    store,
                    data_dir,
                    classifier: Mutex::new(Box::new(classifier)),
                    classification_owner: Uuid::new_v4().to_string(),
                    classification: Arc::new(ClassificationScheduler::default()),
                    mail_rebuilds: Mutex::new(mail_rebuilds),
                    mail_rebuild_running: Mutex::new(HashSet::new()),
                    mail_rebuild_cancellations: MailRebuildCancellations::default(),
                    account_operations: AccountOperationLocks::default(),
                    remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
                    translation_downloads: Mutex::new(HashMap::new()),
                }))
            })?;
            app.manage(state.clone());
            #[cfg(feature = "native-mail-safety-fixture")]
            native_acl_acceptance::install(app, state.clone())?;
            let classification_state = Arc::downgrade(&state);
            state
                .realtime
                .set_hydration_complete_hook(Arc::new(move || {
                    if let Some(state) = classification_state.upgrade() {
                        kick_classification(state);
                    }
                }));
            if release_smoke_test {
                eprintln!("DAKIA_RELEASE_SMOKE_TEST_OK");
                app.handle().exit(0);
                return Ok(());
            }
            #[cfg(feature = "native-mail-safety-fixture")]
            let native_fixture_starts_after_frontend_subscription =
                native_mail_safety_fixture.is_some();
            #[cfg(not(feature = "native-mail-safety-fixture"))]
            let native_fixture_starts_after_frontend_subscription = false;
            // Real-time mail is a native application responsibility. Starting
            // it here keeps delivery alive even when the webview is slow to
            // mount, hidden by --background, or temporarily unavailable.
            let realtime_app = app.handle().clone();
            if !native_fixture_starts_after_frontend_subscription {
                tauri::async_runtime::spawn(async move {
                    kick_classification(state.clone());
                    let rebuilding: HashMap<_, _> = state
                        .mail_rebuilds
                        .lock()
                        .expect("mail rebuild lock poisoned")
                        .iter()
                        .map(|(account_id, job)| (*account_id, job.reset_before_sync))
                        .collect();
                    match state.store.accounts().await {
                        Ok(accounts) => {
                            for account in accounts {
                                if let Some(reset_before_sync) = rebuilding.get(&account.id) {
                                    let rebuild_app = realtime_app.clone();
                                    let rebuild_state = state.clone();
                                    let reset_before_sync = *reset_before_sync;
                                    if !reserve_mail_rebuild(&rebuild_state, account.id) {
                                        continue;
                                    }
                                    tauri::async_runtime::spawn(async move {
                                        if let Err(error) = run_mail_rebuild(
                                            rebuild_app,
                                            rebuild_state,
                                            account,
                                            reset_before_sync,
                                        )
                                        .await
                                        {
                                            tracing::error!(
                                                error = %error,
                                                "could not resume interrupted mail rebuild"
                                            );
                                        }
                                    });
                                } else if account.enabled {
                                    state
                                        .realtime
                                        .start_account(realtime_app.clone(), account)
                                        .await;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(error = %error, "could not start native mail tasks");
                        }
                    }
                });
            }
            if std::env::args().any(|argument| argument == "--background") {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            provider_presets,
            message_content,
            configure_tray,
            terminal_command_status,
            install_terminal_command,
            remove_terminal_command,
            message_attachments,
            save_attachment,
            export_message,
            save_all_attachments,
            forward_attachments,
            read_dropped_files,
            accounts,
            update_account,
            show_account_context_menu,
            show_email_address_context_menu,
            remove_account,
            open_external_url,
            open_compose_window,
            list_send_recoveries,
            open_send_recovery_window,
            read_send_recovery_seed,
            acknowledge_send_recovery_seed,
            quit_startup_recovery,
            open_reader_window,
            open_settings_window,
            open_account_window,
            add_account,
            search,
            search_smart_inbox,
            conversation_for_target,
            search_remote,
            set_message_category,
            set_message_starred,
            set_message_read,
            starred_conversation_count,
            classify_pending,
            start_realtime_sync,
            reconcile_realtime_sync,
            realtime_sync_status,
            record_notification_delivered,
            hydrate_message,
            mail_rebuild_status,
            sync_account,
            send_message,
            apply_mailbox_action,
            unsubscribe_message,
            trash_messages_from_sender,
            ai_summarize,
            ai_draft,
            ai_available,
            send_desktop_notification,
            set_ai_api_key,
            translation_models,
            translation_model_files,
            translation_detect_language,
            translation_install_model,
            translation_cancel_install,
            translation_remove_model
        ])
        .build(app_context())
        .expect("Dakia desktop runtime failed");

    // The pinned macOS runtime currently turns RequestExit(1) into Tao's
    // zero-valued ControlFlow::Exit. Retain only Dakia's startup-failure status,
    // let run_return perform normal Tauri cleanup, then exit from the main thread.
    let runtime_exit_code = app.run_return(|_handle, _event| {});
    std::process::exit(startup_failure_exit_status.resolve(runtime_exit_code));
}
