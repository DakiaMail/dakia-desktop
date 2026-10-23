//! Foreground interaction behavior while one account's provider work holds its
//! production operation lock.

use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use dakia_core::{AccountAuth, Security};
use serde_json::{json, Value};
use std::{
    env, fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex as StdMutex,
    },
    time::Duration,
};
use tauri::{
    ipc::{CallbackFn, InvokeBody, InvokeResponseBody},
    test::{get_ipc_response, mock_builder, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewUrl, WebviewWindowBuilder,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::oneshot,
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{
    rustls::{
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        ServerConfig,
    },
    TlsAcceptor,
};

const TEST_CA_DER_BASE64: &str = "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";
const TEST_CERT_DER_BASE64: &str = "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=";
const TEST_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L";
const PRIMARY_ACCOUNT_ID: &str = "62a9320c-04f5-47c2-b4fa-d4952f0b6121";
const PRIMARY_ACCOUNT_EMAIL: &str = "fictional-reader@localhost";
const SECONDARY_ACCOUNT_ID: &str = "62a9320c-04f5-47c2-b4fa-d4952f0b6122";
const SECONDARY_ACCOUNT_EMAIL: &str = "fictional-reader-two@localhost";
const FIXTURE_PASSWORD: &str = "synthetic-only-password";
const CHILD_ENV: &str = "DAKIA_L011_SEND_CHILD";
const FIXTURE_ENV_NAMES: [&str; 10] = [
    "DAKIA_NATIVE_MAIL_SAFETY_FIXTURE",
    "DAKIA_NATIVE_MAIL_SAFETY_ROOT",
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST",
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT",
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME",
    "DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH",
    "DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT",
];

struct FixtureEnvironment {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl FixtureEnvironment {
    fn install(root: &std::path::Path, smtp_port: u16, imap_port: u16) -> Self {
        let previous = FIXTURE_ENV_NAMES
            .iter()
            .map(|name| (*name, env::var_os(name)))
            .collect::<Vec<_>>();
        let ca_path = root.join("fixture-ca.der");
        fs::write(
            root.join(".dakia-native-mail-safety-root"),
            "dakia-native-mail-safety-fixture-v1\n",
        )
        .expect("write fixture ownership marker");
        fs::write(
            &ca_path,
            STANDARD
                .decode(TEST_CA_DER_BASE64)
                .expect("decode fixture CA"),
        )
        .expect("write fixture CA");

        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE", "1");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_ROOT", root);
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST", "127.0.0.1");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT", smtp_port.to_string());
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME", "localhost");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST", "127.0.0.1");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT", imap_port.to_string());
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME", "localhost");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH", ca_path);
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT", "1");
        Self { previous }
    }
}

impl Drop for FixtureEnvironment {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            if let Some(value) = value {
                env::set_var(name, value);
            } else {
                env::remove_var(name);
            }
        }
    }
}

struct NeverClassifier;

impl EmailClassifier for NeverClassifier {
    fn classify(
        &mut self,
        _emails: &[EmailClassificationInput],
    ) -> anyhow::Result<Vec<dakia_core::classification::ModelClassification>> {
        panic!("held-sync interaction acceptance must not classify mail")
    }
}

fn app_state(store: Store) -> Arc<AppState> {
    Arc::new(AppState {
        realtime: RealtimeSyncManager::new(store.clone()),
        store,
        data_dir: PathBuf::new(),
        classifier: Mutex::new(Box::new(NeverClassifier)),
        classification_owner: "l011-held-sync-interaction".into(),
        classification: Arc::new(ClassificationScheduler::default()),
        remote_operation_slots: Arc::new(Semaphore::new(MESSAGE_HYDRATION_CONCURRENCY)),
        mail_rebuilds: Mutex::new(HashMap::new()),
        mail_rebuild_running: Mutex::new(HashSet::new()),
        mail_rebuild_cancellations: MailRebuildCancellations::default(),
        account_operations: AccountOperationLocks::default(),
        translation_downloads: Mutex::new(HashMap::new()),
    })
}

fn fictional_account(id: Uuid, email: &str) -> Account {
    let mut account = AccountDraft {
        email: email.into(),
        display_name: "Fictional held-sync reader".into(),
        provider_id: Some("custom".into()),
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
    .into_account(provider::by_id("custom").expect("custom provider"));
    account.id = id;
    account
}

fn cached_message(account: &Account) -> MailSummary {
    MailSummary {
        id: "l011-cached-reader-message".into(),
        account_id: account.id.to_string(),
        mailbox: "INBOX".into(),
        uid: 41,
        mailbox_uid_validity: Some(77),
        message_id: Some("<l011-cached-reader-message@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: "l011-cached-reader-thread".into(),
        subject: "Cached Reader content during sync".into(),
        from_name: Some("Fictional provider".into()),
        from_address: "sender@example.test".into(),
        to_addresses: account.email.clone(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: Utc::now(),
        snippet: "Already persisted locally".into(),
        body_text: String::new(),
        body_html: None,
        content_state: "complete".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: true,
        is_flagged: false,
        has_attachments: false,
        category: Some("people".into()),
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: Vec::new(),
    }
}

fn request(command: &str, body: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: command.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("local Tauri URL"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

fn response_json(body: InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(value) => {
            serde_json::from_str(&value).expect("deserialize command response")
        }
        InvokeResponseBody::Raw(_) => panic!("held-sync command returned raw bytes"),
    }
}

fn tls_acceptor() -> TlsAcceptor {
    let certificate = CertificateDer::from(
        STANDARD
            .decode(TEST_CERT_DER_BASE64)
            .expect("decode fixture certificate"),
    );
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        STANDARD
            .decode(TEST_KEY_DER_BASE64)
            .expect("decode fixture key"),
    ));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("build fixture TLS server");
    TlsAcceptor::from(Arc::new(config))
}

fn provider_account(id: &str, email: &str, smtp_port: u16, imap_port: u16) -> Account {
    let mut account = fictional_account(Uuid::parse_str(id).expect("fixture account UUID"), email);
    account.auth = AccountAuth::Password {
        username: email.into(),
    };
    account.smtp_host = "127.0.0.1".into();
    account.smtp_port = smtp_port;
    account.smtp_security = Security::Tls;
    account.imap_host = "127.0.0.1".into();
    account.imap_port = imap_port;
    account.imap_security = Security::Tls;
    account
}

async fn smtp_line<S>(connection: &mut BufReader<S>) -> String
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    assert_ne!(
        connection
            .read_line(&mut line)
            .await
            .expect("read fictional SMTP command"),
        0,
        "SMTP client disconnected before completing the transaction"
    );
    line
}

async fn smtp_reply<S>(connection: &mut BufReader<S>, reply: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    connection
        .get_mut()
        .write_all(reply.as_bytes())
        .await
        .expect("write fictional SMTP response");
    connection
        .get_mut()
        .flush()
        .await
        .expect("flush fictional SMTP response");
}

async fn serve_smtp_session(
    listener: &TcpListener,
    acceptor: &TlsAcceptor,
    accepted: &AtomicUsize,
    completed: &AtomicUsize,
) -> String {
    let (stream, _) = listener.accept().await.expect("accept SMTP session");
    accepted.fetch_add(1, Ordering::SeqCst);
    let stream = acceptor.accept(stream).await.expect("accept SMTP TLS");
    let mut connection = BufReader::new(stream);
    smtp_reply(&mut connection, "220 localhost fictional SMTP ready\r\n").await;

    let sender = loop {
        let command = smtp_line(&mut connection).await;
        if command.starts_with("EHLO ") {
            smtp_reply(
                &mut connection,
                "250-localhost\r\n250-AUTH PLAIN\r\n250 8BITMIME\r\n",
            )
            .await;
        } else if command.starts_with("AUTH PLAIN ") {
            smtp_reply(&mut connection, "235 2.7.0 authenticated\r\n").await;
        } else if let Some(sender) = command
            .strip_prefix("MAIL FROM:<")
            .and_then(|value| value.split('>').next())
        {
            smtp_reply(&mut connection, "250 2.1.0 sender accepted\r\n").await;
            break sender.to_owned();
        } else {
            panic!("unexpected SMTP command before MAIL FROM: {command:?}");
        }
    };
    assert!(
        [PRIMARY_ACCOUNT_EMAIL, SECONDARY_ACCOUNT_EMAIL].contains(&sender.as_str()),
        "fixture accepted an unexpected envelope sender: {sender}"
    );
    assert!(smtp_line(&mut connection).await.starts_with("RCPT TO:<"));
    smtp_reply(&mut connection, "250 2.1.5 recipient accepted\r\n").await;
    assert_eq!(smtp_line(&mut connection).await, "DATA\r\n");
    smtp_reply(&mut connection, "354 send message\r\n").await;
    loop {
        if smtp_line(&mut connection).await == ".\r\n" {
            break;
        }
    }
    smtp_reply(&mut connection, "250 2.0.0 queued\r\n").await;
    completed.fetch_add(1, Ordering::SeqCst);
    sender
}

fn empty_select(tag: &str) -> String {
    format!(
        "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 1] next\r\n{tag} OK selected\r\n"
    )
}

async fn serve_blocked_sync_session(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) {
    let stream = acceptor.accept(stream).await.expect("accept sync IMAP TLS");
    let mut connection = BufReader::new(stream);
    smtp_reply(&mut connection, "* OK localhost fictional IMAP ready\r\n").await;
    assert_eq!(smtp_line(&mut connection).await, "D0001 CAPABILITY\r\n");
    smtp_reply(
        &mut connection,
        "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
    )
    .await;
    let login = smtp_line(&mut connection).await;
    assert!(login.contains(PRIMARY_ACCOUNT_EMAIL));
    smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;

    assert_eq!(smtp_line(&mut connection).await, "D0003 SELECT INBOX\r\n");
    entered.send(()).expect("publish sync provider barrier");
    release.await.expect("release sync provider barrier");
    smtp_reply(&mut connection, &empty_select("D0003")).await;
    for tag in ["D0004", "D0005"] {
        assert_eq!(
            smtp_line(&mut connection).await,
            format!("{tag} SELECT INBOX\r\n")
        );
        smtp_reply(&mut connection, &empty_select(tag)).await;
    }
    assert_eq!(smtp_line(&mut connection).await, "D0006 LOGOUT\r\n");
    smtp_reply(
        &mut connection,
        "* BYE fictional sync done\r\nD0006 OK logout\r\n",
    )
    .await;
}

async fn serve_post_sync_imap_session(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    roles: Arc<StdMutex<Vec<String>>>,
) -> String {
    let stream = acceptor
        .accept(stream)
        .await
        .expect("accept post-sync IMAP TLS");
    let mut connection = BufReader::new(stream);
    smtp_reply(&mut connection, "* OK localhost fictional IMAP ready\r\n").await;
    assert_eq!(smtp_line(&mut connection).await, "D0001 CAPABILITY\r\n");
    smtp_reply(
        &mut connection,
        "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
    )
    .await;
    let login = smtp_line(&mut connection).await;
    let sender = [PRIMARY_ACCOUNT_EMAIL, SECONDARY_ACCOUNT_EMAIL]
        .into_iter()
        .find(|email| login.contains(&format!("\"{email}\"")))
        .expect("post-sync login uses one allowlisted account")
        .to_owned();
    smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
    assert_eq!(
        smtp_line(&mut connection).await,
        "D0003 LIST \"\" \"*\"\r\n"
    );
    smtp_reply(
        &mut connection,
        "* LIST (\\Sent) \"/\" \"Sent\"\r\nD0003 OK listed\r\n",
    )
    .await;
    let command = smtp_line(&mut connection).await;
    if command.starts_with("D0004 APPEND \"Sent\" (\\Seen) {") {
        let byte_count = command
            .rsplit_once('{')
            .and_then(|(_, suffix)| suffix.strip_suffix("}\r\n"))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("APPEND declares a valid literal length");
        smtp_reply(&mut connection, "+ continue\r\n").await;
        let mut message = vec![0_u8; byte_count];
        connection
            .read_exact(&mut message)
            .await
            .expect("read complete Sent copy");
        let mut trailing_crlf = [0_u8; 2];
        connection
            .read_exact(&mut trailing_crlf)
            .await
            .expect("read APPEND literal terminator");
        assert_eq!(trailing_crlf, *b"\r\n");
        smtp_reply(&mut connection, "D0004 OK appended\r\n").await;
        assert_eq!(smtp_line(&mut connection).await, "D0005 LOGOUT\r\n");
        smtp_reply(
            &mut connection,
            "* BYE fictional IMAP done\r\nD0005 OK logout\r\n",
        )
        .await;
        let role = format!("sent:{sender}");
        roles.lock().expect("IMAP roles lock").push(role.clone());
        role
    } else {
        assert_eq!(sender, PRIMARY_ACCOUNT_EMAIL);
        assert!(
            command.starts_with("D0004 SELECT "),
            "unexpected realtime watcher command: {command:?}"
        );
        roles
            .lock()
            .expect("IMAP roles lock")
            .push("watcher:primary".into());
        let mut discarded = String::new();
        let read = timeout(Duration::from_secs(3), connection.read_line(&mut discarded))
            .await
            .expect("realtime watcher cancellation closed its socket")
            .expect("read realtime watcher cancellation");
        assert_eq!(read, 0, "cancelled watcher sent another command");
        "watcher:primary".into()
    }
}

async fn run_isolated_test(test_name: &'static str, child_mode: &'static str) {
    let executable = env::current_exe().expect("resolve current Rust test executable");
    let mut command = Command::new(executable);
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_ENV, child_mode)
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .expect("isolated held-sync send test exceeded its hard timeout")
        .expect("spawn isolated held-sync send test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated held-sync send test failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("running 1 test")
            && stdout.contains(&format!("test {test_name} ... ok"))
            && stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "isolated child exited without proving the requested test ran once\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

fn send_request(account_id: Uuid, subject: &str, attempt_id: Uuid) -> InvokeRequest {
    request(
        "send_message",
        json!({
            "draft": {
                "account_id": account_id,
                "to": ["recipient@example.test"],
                "cc": [],
                "bcc": [],
                "subject": subject,
                "body_text": "Fictional foreground interaction body",
                "body_html": null,
                "in_reply_to": null,
                "references": null,
                "attachments": [],
                "send_attempt_id": attempt_id,
            },
        }),
    )
}

#[tokio::test]
async fn cached_reader_content_completes_while_the_same_account_sync_lock_is_held() {
    const TEST_NAME: &str = "l011_sync_interaction_tests::cached_reader_content_completes_while_the_same_account_sync_lock_is_held";
    if env::var(CHILD_ENV).as_deref() != Ok("reader-content") {
        run_isolated_test(TEST_NAME, "reader-content").await;
        return;
    }

    let store = Store::in_memory().await.expect("open fixture store");
    let account = fictional_account(Uuid::new_v4(), "reader@example.test");
    let message = cached_message(&account);
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 42, true)
        .await
        .expect("save mailbox epoch");
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("save cached message catalogue");
    store
        .cache_message_content(
            &message.id,
            false,
            CachedMessageContent {
                body_text: "Cached body remains available".into(),
                body_html: Some("<p>Cached body remains available</p>".into()),
                unsubscribe_kind: None,
                attachments: Vec::new(),
            },
        )
        .await
        .expect("cache Reader body");

    let state = app_state(store);
    let sync_guard = state.account_operations.acquire(account.id).await;
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![message_content])
        .build(super::test_support::mock_context())
        .expect("build production command dispatcher");
    let reader = WebviewWindowBuilder::new(&app, "reader-l011-held-sync", WebviewUrl::default())
        .build()
        .expect("build Reader window");

    let (response_tx, response_rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        response_tx
            .send(get_ipc_response(
                &reader,
                request(
                    "message_content",
                    json!({
                        "messageId": message.id,
                        "expectedUidValidity": 77,
                    }),
                ),
            ))
            .expect("return cached Reader response");
    });
    let content = response_json(
        tokio::task::spawn_blocking(move || response_rx.recv_timeout(Duration::from_secs(2)))
            .await
            .expect("Reader response waiter did not panic")
            .expect("cached Reader content must not wait for provider sync")
            .expect("cached Reader content succeeds"),
    );
    assert_eq!(content["body_text"], "Cached body remains available");
    assert_eq!(content["body_html"], "<p>Cached body remains available</p>");
    drop(sync_guard);
    reader_thread.join().expect("Reader command thread");
}

#[tokio::test]
async fn held_sync_blocks_only_that_accounts_send_and_each_account_submits_once() {
    const TEST_NAME: &str = "l011_sync_interaction_tests::held_sync_blocks_only_that_accounts_send_and_each_account_submits_once";
    if env::var(CHILD_ENV).as_deref() != Ok("account-isolation") {
        run_isolated_test(TEST_NAME, "account-isolation").await;
        return;
    }

    let smtp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional SMTP provider");
    let smtp_port = smtp_listener
        .local_addr()
        .expect("SMTP provider address")
        .port();
    let imap_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional Sent provider");
    let imap_port = imap_listener
        .local_addr()
        .expect("IMAP provider address")
        .port();
    let root = tempfile::tempdir().expect("create marker-owned fixture root");
    let _environment = FixtureEnvironment::install(root.path(), smtp_port, imap_port);
    let smtp_acceptor = tls_acceptor();
    let imap_acceptor = tls_acceptor();
    let accepted = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let imap_roles = Arc::new(StdMutex::new(Vec::new()));
    let (sync_entered_tx, sync_entered_rx) = oneshot::channel();
    let (sync_release_tx, sync_release_rx) = oneshot::channel();
    let server_accepted = accepted.clone();
    let server_completed = completed.clone();
    let smtp_server = tokio::spawn(async move {
        let first = serve_smtp_session(
            &smtp_listener,
            &smtp_acceptor,
            &server_accepted,
            &server_completed,
        )
        .await;
        let second = serve_smtp_session(
            &smtp_listener,
            &smtp_acceptor,
            &server_accepted,
            &server_completed,
        )
        .await;
        assert!(
            timeout(Duration::from_millis(200), smtp_listener.accept())
                .await
                .is_err(),
            "a duplicate third SMTP submission reached the provider"
        );
        vec![first, second]
    });
    let server_roles = imap_roles.clone();
    let imap_server = tokio::spawn(async move {
        let (sync_stream, _) = timeout(Duration::from_secs(3), imap_listener.accept())
            .await
            .expect("sync command reached the IMAP provider")
            .expect("accept sync IMAP session");
        let sync_session = tokio::spawn(serve_blocked_sync_session(
            sync_stream,
            imap_acceptor.clone(),
            sync_entered_tx,
            sync_release_rx,
        ));
        let mut sessions = JoinSet::new();
        for _ in 0..2 {
            let (stream, _) = timeout(Duration::from_secs(5), imap_listener.accept())
                .await
                .expect("expected Sent or realtime IMAP session")
                .expect("accept post-sync IMAP session");
            sessions.spawn(serve_post_sync_imap_session(
                stream,
                imap_acceptor.clone(),
                server_roles.clone(),
            ));
        }
        let mut observed = Vec::new();
        while let Some(result) = sessions.join_next().await {
            observed.push(result.expect("post-sync IMAP session did not panic"));
        }
        sync_session.await.expect("sync IMAP session did not panic");
        assert!(
            timeout(Duration::from_millis(200), imap_listener.accept())
                .await
                .is_err(),
            "an unexpected third post-sync IMAP session reached the provider"
        );
        observed
    });

    let primary = provider_account(
        PRIMARY_ACCOUNT_ID,
        PRIMARY_ACCOUNT_EMAIL,
        smtp_port,
        imap_port,
    );
    let secondary = provider_account(
        SECONDARY_ACCOUNT_ID,
        SECONDARY_ACCOUNT_EMAIL,
        smtp_port,
        imap_port,
    );
    let store = Store::in_memory().await.expect("open fixture store");
    for account in [&primary, &secondary] {
        store
            .save_account(account)
            .await
            .expect("save fictional SMTP account");
        MailService::new(store.clone())
            .credentials()
            .set_password(account, FIXTURE_PASSWORD)
            .await
            .expect("save fictional SMTP credential");
    }
    let reader_message = cached_message(&primary);
    store
        .save_mailbox_catalog_state(primary.id, "INBOX", "INBOX", 77, 42, true)
        .await
        .expect("save Reader mailbox epoch");
    store
        .upsert_messages(std::slice::from_ref(&reader_message))
        .await
        .expect("save cached Reader message");
    store
        .cache_message_content(
            &reader_message.id,
            false,
            CachedMessageContent {
                body_text: "Cached body remains available".into(),
                body_html: Some("<p>Cached body remains available</p>".into()),
                unsubscribe_kind: None,
                attachments: Vec::new(),
            },
        )
        .await
        .expect("cache Reader content");
    let state = app_state(store);
    let app = mock_builder()
        .manage(state.clone())
        .manage(Arc::new(SendRecoveryRuntime::default()))
        .invoke_handler(tauri::generate_handler![message_content, send_message])
        .build(super::test_support::mock_context())
        .expect("build production send command dispatcher");
    let reader = WebviewWindowBuilder::new(&app, "reader-l011-sync", WebviewUrl::default())
        .build()
        .expect("build Reader window");
    let primary_compose =
        WebviewWindowBuilder::new(&app, "compose-l011-primary", WebviewUrl::default())
            .build()
            .expect("build primary Compose window");
    let secondary_compose =
        WebviewWindowBuilder::new(&app, "compose-l011-secondary", WebviewUrl::default())
            .build()
            .expect("build secondary Compose window");

    let primary_id = primary.id;
    let restart_calls = Arc::new(AtomicUsize::new(0));
    let sync_state = state.clone();
    let sync_restart_calls = restart_calls.clone();
    let sync_task = tokio::spawn(async move {
        run_incremental_sync_account(
            &sync_state,
            primary_id,
            50,
            |_| {},
            || async {
                assert_eq!(
                    sync_state.account_operations.retained_users(primary_id),
                    2,
                    "restart runs while sync owns the account lock and send waits"
                );
                sync_restart_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
    });
    timeout(Duration::from_secs(3), sync_entered_rx)
        .await
        .expect("production sync reached its provider barrier")
        .expect("sync provider barrier remained available");
    assert_eq!(state.account_operations.retained_users(primary.id), 1);

    let reader_content = response_json(
        timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                get_ipc_response(
                    &reader,
                    request(
                        "message_content",
                        json!({
                            "messageId": reader_message.id,
                            "expectedUidValidity": 77,
                        }),
                    ),
                )
            }),
        )
        .await
        .expect("cached Reader command must not wait for production sync")
        .expect("Reader command worker did not panic")
        .expect("cached Reader content completes during production sync"),
    );
    assert_eq!(reader_content["body_text"], "Cached body remains available");

    let (primary_tx, primary_rx) = std::sync::mpsc::channel();
    let primary_request = send_request(primary.id, "Primary waits for sync", Uuid::new_v4());
    let primary_thread = std::thread::spawn(move || {
        primary_tx
            .send(get_ipc_response(&primary_compose, primary_request))
            .expect("return primary send response");
    });
    timeout(Duration::from_secs(2), async {
        while state.account_operations.retained_users(primary.id) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same-account send registered behind held sync lock");
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    let (secondary_tx, secondary_rx) = std::sync::mpsc::channel();
    let secondary_request = send_request(
        secondary.id,
        "Other account stays responsive",
        Uuid::new_v4(),
    );
    let secondary_thread = std::thread::spawn(move || {
        secondary_tx
            .send(get_ipc_response(&secondary_compose, secondary_request))
            .expect("return secondary send response");
    });
    let secondary_response =
        tokio::task::spawn_blocking(move || secondary_rx.recv_timeout(Duration::from_secs(3)))
            .await
            .expect("secondary response waiter did not panic")
            .expect("other-account send completes while primary sync is held")
            .expect("other-account send command succeeds");
    assert_eq!(response_json(secondary_response), "2.0.0 queued");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(matches!(
        primary_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    sync_release_tx
        .send(())
        .expect("release production sync provider barrier");
    let (sync_result, synced_account) = timeout(Duration::from_secs(3), sync_task)
        .await
        .expect("production sync helper completes after provider release")
        .expect("production sync task did not panic")
        .expect("production sync helper resolves its account");
    assert_eq!(synced_account.id, primary.id);
    assert_eq!(
        sync_result.expect("production sync succeeds").synced_count,
        0
    );
    assert_eq!(restart_calls.load(Ordering::SeqCst), 1);

    let primary_response =
        tokio::task::spawn_blocking(move || primary_rx.recv_timeout(Duration::from_secs(3)))
            .await
            .expect("primary response waiter did not panic")
            .expect("same-account send completes after sync releases")
            .expect("same-account send command succeeds");
    assert_eq!(response_json(primary_response), "2.0.0 queued");
    primary_thread.join().expect("primary send thread");
    secondary_thread.join().expect("secondary send thread");

    let smtp_senders = timeout(Duration::from_secs(2), smtp_server)
        .await
        .expect("SMTP provider test completed")
        .expect("SMTP provider task did not panic");
    let imap_sessions = timeout(Duration::from_secs(3), imap_server)
        .await
        .expect("IMAP provider test completed")
        .expect("IMAP provider task did not panic");
    assert_eq!(
        smtp_senders,
        [SECONDARY_ACCOUNT_EMAIL, PRIMARY_ACCOUNT_EMAIL]
    );
    assert_eq!(
        imap_sessions
            .iter()
            .filter(|role| role.starts_with("sent:"))
            .count(),
        2
    );
    assert!(imap_sessions.contains(&"sent:fictional-reader@localhost".into()));
    assert!(imap_sessions.contains(&"sent:fictional-reader-two@localhost".into()));
    assert!(!imap_sessions
        .iter()
        .any(|role| role.starts_with("watcher:")));
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}
