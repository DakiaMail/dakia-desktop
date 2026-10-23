use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use dakia_core::{AccountAuth, Security};
use std::{
    env, fs,
    sync::{atomic::AtomicUsize, MutexGuard},
};
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
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
const FIXTURE_ACCOUNT_ID: &str = "62a9320c-04f5-47c2-b4fa-d4952f0b6121";
const FIXTURE_ACCOUNT_EMAIL: &str = "fictional-reader@localhost";
const FIXTURE_PASSWORD: &str = "realtime secret";
const CHILD_ENV: &str = "DAKIA_L010_REAL_TLS_CHILD";
const FIXTURE_ENV_NAMES: [&str; 7] = [
    "DAKIA_NATIVE_MAIL_SAFETY_FIXTURE",
    "DAKIA_NATIVE_MAIL_SAFETY_ROOT",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT",
    "DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME",
    "DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH",
    "DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT",
];

static FIXTURE_ENV_LOCK: StdMutex<()> = StdMutex::new(());

struct FixtureEnvironment {
    _lock: MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl FixtureEnvironment {
    fn install(root: &std::path::Path, port: u16) -> Self {
        let lock = FIXTURE_ENV_LOCK
            .lock()
            .expect("fixture environment lock is not poisoned");
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
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST", "127.0.0.1");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT", port.to_string());
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME", "localhost");
        env::set_var("DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH", ca_path);
        env::remove_var("DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT");

        Self {
            _lock: lock,
            previous,
        }
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

#[derive(Clone, Copy)]
enum ProviderSession {
    IdleDisconnect,
    RateLimited,
    FragmentedPoll,
    AuthenticationRejected,
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

fn fixture_account(port: u16) -> Account {
    let mut account = realtime_account();
    account.id = Uuid::parse_str(FIXTURE_ACCOUNT_ID).expect("fixture UUID");
    account.email = FIXTURE_ACCOUNT_EMAIL.into();
    account.display_name = "Fictional realtime reader".into();
    account.auth = AccountAuth::Password {
        username: FIXTURE_ACCOUNT_EMAIL.into(),
    };
    account.imap_host = "127.0.0.1".into();
    account.imap_port = port;
    account.imap_security = Security::Tls;
    account
}

async fn read_command<R>(read: &mut BufReader<R>) -> (String, String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    let count = read
        .read_line(&mut line)
        .await
        .expect("read fictional provider command");
    assert_ne!(
        count, 0,
        "provider connection closed before the next command"
    );
    let line = line.strip_suffix("\r\n").expect("IMAP command uses CRLF");
    let (tag, command) = line.split_once(' ').expect("IMAP command includes a tag");
    (tag.to_owned(), command.to_owned())
}

async fn write_response<W>(write: &mut W, response: &str, fragmented: bool)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if fragmented && response.len() > 3 {
        let first = 1;
        let second = response.len() / 2;
        for chunk in [
            &response.as_bytes()[..first],
            &response.as_bytes()[first..second],
            &response.as_bytes()[second..],
        ] {
            write
                .write_all(chunk)
                .await
                .expect("write fragmented provider response");
            write.flush().await.expect("flush response fragment");
            tokio::task::yield_now().await;
        }
    } else {
        write
            .write_all(response.as_bytes())
            .await
            .expect("write provider response");
        write.flush().await.expect("flush provider response");
    }
}

async fn serve_session(
    listener: &TcpListener,
    acceptor: &TlsAcceptor,
    script: ProviderSession,
) -> Vec<String> {
    let (stream, _) = listener.accept().await.expect("accept provider session");
    let stream = acceptor
        .accept(stream)
        .await
        .expect("accept provider TLS session");
    let (read, mut write) = split(stream);
    let mut read = BufReader::new(read);
    let fragmented = matches!(script, ProviderSession::FragmentedPoll);
    let capability = if fragmented {
        "IMAP4rev1"
    } else {
        "IMAP4rev1 IDLE"
    };
    let mut authenticated = false;
    let mut selects = 0_u8;
    let mut transcript = Vec::new();

    write_response(
        &mut write,
        "* OK fictional realtime provider ready\r\n",
        fragmented,
    )
    .await;
    loop {
        let (tag, command) = read_command(&mut read).await;
        transcript.push(command.clone());
        if command == "CAPABILITY" {
            if authenticated && matches!(script, ProviderSession::RateLimited) {
                write_response(
                    &mut write,
                    &format!("{tag} NO [LIMIT] provider 429 rate limit\r\n"),
                    false,
                )
                .await;
            } else {
                write_response(
                    &mut write,
                    &format!("* CAPABILITY {capability}\r\n{tag} OK capability\r\n"),
                    fragmented,
                )
                .await;
            }
        } else if command == format!("LOGIN \"{FIXTURE_ACCOUNT_EMAIL}\" \"{FIXTURE_PASSWORD}\"") {
            if matches!(script, ProviderSession::AuthenticationRejected) {
                write_response(
                    &mut write,
                    &format!("{tag} NO authentication rejected\r\n"),
                    false,
                )
                .await;
                break;
            }
            authenticated = true;
            write_response(
                &mut write,
                &format!("{tag} OK authenticated\r\n"),
                fragmented,
            )
            .await;
        } else if command == "SELECT INBOX" {
            selects += 1;
            write_response(
                &mut write,
                &format!(
                    "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] fictional epoch\r\n* OK [UIDNEXT 43] next\r\n{tag} OK selected\r\n"
                ),
                fragmented,
            )
            .await;
        } else if command == "IDLE" {
            assert!(
                matches!(script, ProviderSession::IdleDisconnect),
                "only the disconnect fixture enters IDLE"
            );
            assert_eq!(selects, 3, "IDLE begins only after reconciliation");
            write_response(
                &mut write,
                "+ idling\r\n* BYE fictional network disconnect\r\n",
                false,
            )
            .await;
            break;
        } else if command == "LOGOUT" {
            write_response(
                &mut write,
                &format!("* BYE done\r\n{tag} OK logout\r\n"),
                fragmented,
            )
            .await;
            break;
        } else {
            panic!("unexpected fictional provider command: {command}");
        }
    }
    transcript
}

async fn spawn_isolated_test(test_name: &'static str, child_mode: &'static str) {
    let executable = env::current_exe().expect("resolve current Rust test executable");
    let mut command = Command::new(executable);
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_ENV, child_mode)
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .expect("isolated real-TLS watcher test exceeded its hard timeout")
        .expect("spawn isolated real-TLS watcher test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated real-TLS watcher test failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("running 1 test")
            && stdout.contains(&format!("test {test_name} ... ok"))
            && stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "isolated test process exited successfully without proving that the requested test ran exactly once\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

async fn fixture_store_and_runner(port: u16) -> (Store, Account, MailServiceRealtimeCycleRunner) {
    let store = Store::in_memory()
        .await
        .expect("open watcher fixture store");
    let account = fixture_account(port);
    store
        .save_account(&account)
        .await
        .expect("save watcher fixture account");
    MailService::new(store.clone())
        .credentials()
        .set_password(&account, FIXTURE_PASSWORD)
        .await
        .expect("save watcher fixture credential");
    let runner = MailServiceRealtimeCycleRunner {
        store: store.clone(),
        account: account.clone(),
    };
    (store, account, runner)
}

#[tokio::test]
async fn real_tls_provider_failures_flow_through_the_production_watcher_without_a_storm() {
    const TEST_NAME: &str = "realtime::tests::l010_tls_tests::real_tls_provider_failures_flow_through_the_production_watcher_without_a_storm";
    if env::var(CHILD_ENV).as_deref() != Ok("retry-sequence") {
        spawn_isolated_test(TEST_NAME, "retry-sequence").await;
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional TLS provider");
    let port = listener.local_addr().expect("provider address").port();
    let root = tempfile::tempdir().expect("create marker-owned fixture root");
    let _environment = FixtureEnvironment::install(root.path(), port);
    let acceptor = tls_acceptor();
    let accepted = Arc::new(AtomicUsize::new(0));
    let server_accepted = accepted.clone();
    let server = tokio::spawn(async move {
        let mut transcripts = Vec::new();
        for script in [
            ProviderSession::IdleDisconnect,
            ProviderSession::RateLimited,
            ProviderSession::FragmentedPoll,
            ProviderSession::RateLimited,
        ] {
            transcripts.push(serve_session(&listener, &acceptor, script).await);
            server_accepted.fetch_add(1, Ordering::SeqCst);
        }
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "a fifth provider connection started while retry backoff was held"
        );
        transcripts
    });

    let (store, account, mut runner) = fixture_store_and_runner(port).await;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let sink = Arc::new(RecordingEventSink::default());
    let (delay, requested, mut entered) = ControlledWatcherDelay::new([
        ControlledDelayStep::Continue,
        ControlledDelayStep::Continue,
        ControlledDelayStep::Continue,
        ControlledDelayStep::WaitForCancellation,
    ]);
    let (cancel, receiver) = watch::channel(false);
    let watcher = tokio::spawn({
        let sink = sink.clone();
        let statuses = statuses.clone();
        let account = account.clone();
        async move {
            run_watcher_with_runner(
                sink,
                store,
                statuses,
                account,
                receiver,
                Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
                Arc::new(StdRwLock::new(None)),
                &mut runner,
                &delay,
                false,
            )
            .await;
        }
    });

    let mut observed_delays = Vec::new();
    for _ in 0..4 {
        observed_delays.push(
            timeout(Duration::from_secs(3), entered.recv())
                .await
                .expect("watcher reaches each controlled delay")
                .expect("controlled delay channel remains open"),
        );
    }
    assert_eq!(
        observed_delays,
        [
            retry_delay(1, account.id),
            retry_delay(2, account.id),
            poll_delay(account.id),
            retry_delay(1, account.id),
        ]
    );
    let transcripts = server.await.expect("fictional provider server");
    assert_eq!(accepted.load(Ordering::SeqCst), 4);
    assert!(transcripts[0].iter().any(|command| command == "IDLE"));
    assert!(
        transcripts[1]
            .iter()
            .filter(|command| command.as_str() == "CAPABILITY")
            .count()
            >= 2
    );
    assert_eq!(
        transcripts[2]
            .iter()
            .filter(|command| command.as_str() == "SELECT INBOX")
            .count(),
        3
    );
    assert!(transcripts[2].iter().all(|command| command != "IDLE"));
    assert!(
        transcripts[3]
            .iter()
            .filter(|command| command.as_str() == "CAPABILITY")
            .count()
            >= 2
    );

    cancel.send(true).expect("cancel watcher in retry backoff");
    timeout(Duration::from_secs(1), watcher)
        .await
        .expect("cancellation releases controlled retry delay")
        .expect("watcher task");
    assert_eq!(*requested.lock().unwrap(), observed_delays);
    let states = sink
        .events()
        .into_iter()
        .filter(|(event, _)| *event == "mail-sync-state")
        .map(|(_, payload)| payload["state"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [
            "connecting",
            "retrying",
            "connecting",
            "retrying",
            "connecting",
            "polling",
            "connecting",
            "retrying",
        ]
    );
}

#[tokio::test]
async fn real_tls_authentication_rejection_pauses_the_production_watcher_without_reconnect() {
    const TEST_NAME: &str = "realtime::tests::l010_tls_tests::real_tls_authentication_rejection_pauses_the_production_watcher_without_reconnect";
    if env::var(CHILD_ENV).as_deref() != Ok("authentication-rejection") {
        spawn_isolated_test(TEST_NAME, "authentication-rejection").await;
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional authentication provider");
    let port = listener.local_addr().expect("provider address").port();
    let root = tempfile::tempdir().expect("create marker-owned fixture root");
    let _environment = FixtureEnvironment::install(root.path(), port);
    let acceptor = tls_acceptor();
    let server = tokio::spawn(async move {
        let transcript = serve_session(
            &listener,
            &acceptor,
            ProviderSession::AuthenticationRejected,
        )
        .await;
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "authentication rejection must not open a second provider connection"
        );
        transcript
    });

    let (store, account, mut runner) = fixture_store_and_runner(port).await;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let sink = Arc::new(RecordingEventSink::default());
    let (delay, requested, _entered) =
        ControlledWatcherDelay::new(std::iter::empty::<ControlledDelayStep>());
    let (_cancel, receiver) = watch::channel(false);

    run_watcher_with_runner(
        sink.clone(),
        store,
        statuses.clone(),
        account.clone(),
        receiver,
        Arc::new(Semaphore::new(HYDRATION_CONCURRENCY)),
        Arc::new(StdRwLock::new(None)),
        &mut runner,
        &delay,
        false,
    )
    .await;

    let transcript = server.await.expect("authentication provider server");
    assert!(transcript
        .iter()
        .any(|command| command.starts_with("LOGIN ")));
    assert!(requested.lock().unwrap().is_empty());
    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1["state"], "connecting");
    assert_eq!(events[1].1["state"], "paused");
    assert_eq!(events[1].1["errorKind"], "authentication");
    assert_eq!(events[1].1["retryAt"], serde_json::Value::Null);
    let final_status = statuses.read().await[&account.id].clone();
    assert_eq!(final_status.state, "paused");
    assert_eq!(final_status.error_kind.as_deref(), Some("authentication"));
    assert!(final_status.retry_at.is_none());
}
