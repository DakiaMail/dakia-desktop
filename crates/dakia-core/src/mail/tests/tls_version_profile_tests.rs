//! Production-connector evidence for Dakia's supported TLS protocol profile.

use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::Duration as StdDuration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    rustls::{
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        version::{TLS12, TLS13},
        ProtocolVersion, ServerConfig, SupportedProtocolVersion,
    },
    TlsAcceptor,
};

fn version_acceptor(version: &'static SupportedProtocolVersion) -> TlsAcceptor {
    let certificate = CertificateDer::from(STANDARD.decode(SMTP_TEST_CERT_DER_BASE64).unwrap());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        STANDARD.decode(SMTP_TEST_KEY_DER_BASE64).unwrap(),
    ));
    let config = ServerConfig::builder_with_protocol_versions(&[version])
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

async fn assert_production_connector_negotiates(
    version: &'static SupportedProtocolVersion,
    expected: ProtocolVersion,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = version_acceptor(version);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let observed = stream.get_ref().1.protocol_version();
        let mut connection = BufReader::new(stream);
        connection
            .get_mut()
            .write_all(b"* OK fictional IMAP ready\r\n")
            .await
            .unwrap();
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        observed
    });

    let account = imap_test_account(Security::Tls, port);
    let client = ImapClient::connect_with_endpoint(&account, imap_test_endpoint(port, true))
        .await
        .unwrap();
    drop(client);
    let observed = server.await.unwrap();
    assert_eq!(observed, Some(expected));
}

#[tokio::test]
async fn production_imap_connector_negotiates_with_tls12_only_server() {
    assert_production_connector_negotiates(&TLS12, ProtocolVersion::TLSv1_2).await;
}

#[tokio::test]
async fn production_imap_connector_negotiates_with_tls13_only_server() {
    assert_production_connector_negotiates(&TLS13, ProtocolVersion::TLSv1_3).await;
}

async fn assert_production_smtp_connector_negotiates(
    security: Security,
    version: &'static SupportedProtocolVersion,
    expected: ProtocolVersion,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = smtp_test_account(security, listener.local_addr().unwrap().port());
    let acceptor = version_acceptor(version);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = match security {
            Security::Tls => acceptor.accept(stream).await.unwrap(),
            Security::StartTls => {
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
                acceptor.accept(plaintext.into_inner()).await.unwrap()
            }
        };
        let observed = stream.get_ref().1.protocol_version();
        let mut connection = BufReader::new(stream);
        if matches!(security, Security::Tls) {
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
        }
        smtp_expect_ehlo_and_auth(&mut connection, false).await;
        assert_eq!(smtp_command(&mut connection).await, "QUIT\r\n");
        smtp_reply(&mut connection, "221 2.0.0 bye\r\n").await;
        observed
    });

    smtp_auth_probe_with_smtp_endpoint(
        &account,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    let observed = server.await.unwrap();
    assert_eq!(observed, Some(expected));
}

#[tokio::test]
async fn production_smtp_implicit_tls_negotiates_with_tls12_only_server() {
    assert_production_smtp_connector_negotiates(Security::Tls, &TLS12, ProtocolVersion::TLSv1_2)
        .await;
}

#[tokio::test]
async fn production_smtp_implicit_tls_negotiates_with_tls13_only_server() {
    assert_production_smtp_connector_negotiates(Security::Tls, &TLS13, ProtocolVersion::TLSv1_3)
        .await;
}

#[tokio::test]
async fn production_smtp_starttls_negotiates_with_tls12_only_server() {
    assert_production_smtp_connector_negotiates(
        Security::StartTls,
        &TLS12,
        ProtocolVersion::TLSv1_2,
    )
    .await;
}

#[tokio::test]
async fn production_smtp_starttls_negotiates_with_tls13_only_server() {
    assert_production_smtp_connector_negotiates(
        Security::StartTls,
        &TLS13,
        ProtocolVersion::TLSv1_3,
    )
    .await;
}

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut output = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        output.push_str(std::str::from_utf8(line).unwrap());
        output.push('\n');
    }
    output.push_str(&format!("-----END {label}-----\n"));
    output
}

const TLS11_CIPHER: &str = "ECDHE-ECDSA-AES128-SHA:@SECLEVEL=0";

struct OpenSslTls11Server {
    child: Child,
    output_path: PathBuf,
    port: u16,
}

impl OpenSslTls11Server {
    fn start(directory: &Path, certificate_path: &Path, key_path: &Path) -> Self {
        let mut last_failure = String::new();
        for attempt in 0..5 {
            let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = reserved.local_addr().unwrap().port();
            drop(reserved);
            let output_path = directory.join(format!("server-output-{attempt}.log"));
            let output = fs::File::create(&output_path).unwrap();
            let mut server = Self {
                child: Command::new("openssl")
                    .args([
                        "s_server",
                        "-accept",
                        &format!("127.0.0.1:{port}"),
                        "-tls1_1",
                        "-cipher",
                        TLS11_CIPHER,
                        "-ign_eof",
                        "-cert",
                        certificate_path.to_str().unwrap(),
                        "-key",
                        key_path.to_str().unwrap(),
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::from(output.try_clone().unwrap()))
                    .stderr(Stdio::from(output))
                    .spawn()
                    .unwrap(),
                output_path,
                port,
            };
            for _ in 0..200 {
                if server.child.try_wait().unwrap().is_some() {
                    break;
                }
                if server.output().contains("ACCEPT") {
                    return server;
                }
                thread::sleep(StdDuration::from_millis(25));
            }
            last_failure = server.output();
        }
        panic!("OpenSSL TLS 1.1 fixture could not start after bounded retries: {last_failure}");
    }

    fn output(&self) -> String {
        fs::read_to_string(&self.output_path).unwrap_or_default()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for OpenSslTls11Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn openssl_tls11_fixture_files(directory: &Path) -> (PathBuf, PathBuf) {
    let help = Command::new("openssl")
        .args(["s_server", "-help"])
        .output()
        .expect("OpenSSL CLI is required for the manual TLS 1.1 control");
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(
        help_text.contains("-tls1_1"),
        "OpenSSL cannot offer TLS 1.1"
    );

    let certificate_path = directory.join("server-cert.pem");
    let key_path = directory.join("server-key.pem");
    fs::write(
        &certificate_path,
        pem(
            "CERTIFICATE",
            &STANDARD.decode(SMTP_TEST_CERT_DER_BASE64).unwrap(),
        ),
    )
    .unwrap();
    fs::write(
        &key_path,
        pem(
            "PRIVATE KEY",
            &STANDARD.decode(SMTP_TEST_KEY_DER_BASE64).unwrap(),
        ),
    )
    .unwrap();
    (certificate_path, key_path)
}

fn assert_openssl_tls11_control(directory: &Path, certificate_path: &Path, key_path: &Path) {
    let control_server = OpenSslTls11Server::start(directory, certificate_path, key_path);
    let control = Command::new("openssl")
        .args([
            "s_client",
            "-connect",
            &format!("127.0.0.1:{}", control_server.port),
            "-tls1_1",
            "-cipher",
            TLS11_CIPHER,
            "-brief",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let control_transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );
    let control_server_transcript = control_server.output();
    assert!(
        control.status.success(),
        "client:\n{control_transcript}\nserver:\n{control_server_transcript}"
    );
    assert!(
        control_transcript.contains("Protocol version: TLSv1.1"),
        "the forced control did not negotiate TLS 1.1: {control_transcript}"
    );
}

/// This is a manual source-lane control because it requires a local OpenSSL
/// binary that can faithfully run a TLS 1.1 server. A skipped default run is
/// not evidence that legacy TLS was rejected.
#[tokio::test]
#[ignore = "requires an OpenSSL CLI with TLS 1.1 server support"]
async fn production_imap_connector_rejects_openssl_tls11_before_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let (certificate_path, key_path) = openssl_tls11_fixture_files(directory.path());
    assert_openssl_tls11_control(directory.path(), &certificate_path, &key_path);

    let mut server = OpenSslTls11Server::start(directory.path(), &certificate_path, &key_path);
    let port = server.port;

    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let account = imap_test_account(Security::Tls, port);
    store.save_account(&account).await.unwrap();
    service
        .credentials()
        .set_password(&account, "tls11-credential-must-not-be-written")
        .await
        .unwrap();
    let endpoint = MailService::imap_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        "127.0.0.1".into(),
        STANDARD.decode(SMTP_TEST_CA_DER_BASE64).unwrap(),
    )
    .unwrap();
    let result = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 1, endpoint)
        .await;

    server.stop();
    let observed = server.output();
    let error = result.expect_err("TLS 1.1 unexpectedly reached IMAP authentication");
    assert!(
        error.to_string().contains("TLS handshake failed"),
        "{error:#}"
    );
    assert!(!format!("{error:#}").contains("tls11-credential"));
    assert!(!observed.contains("tls11-credential-must-not-be-written"));
}

/// This is a manual source-lane control because it requires a local OpenSSL
/// binary that can faithfully run a TLS 1.1 server. A skipped default run is
/// not evidence that legacy TLS was rejected.
#[tokio::test]
#[ignore = "requires an OpenSSL CLI with TLS 1.1 server support"]
async fn production_smtp_connector_rejects_openssl_tls11_before_credentials_or_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let (certificate_path, key_path) = openssl_tls11_fixture_files(directory.path());
    assert_openssl_tls11_control(directory.path(), &certificate_path, &key_path);

    let mut server = OpenSslTls11Server::start(directory.path(), &certificate_path, &key_path);
    let account = smtp_test_account(Security::Tls, server.port);
    let endpoint = MailService::smtp_loopback_endpoint(
        "127.0.0.1".into(),
        server.port,
        "127.0.0.1".into(),
        STANDARD.decode(SMTP_TEST_CA_DER_BASE64).unwrap(),
    )
    .unwrap();
    let result = smtp_auth_probe_with_smtp_endpoint(
        &account,
        "smtp-tls11-credential-must-not-be-written",
        endpoint,
        Duration::from_secs(2),
    )
    .await;

    server.stop();
    let observed = server.output();
    let error = result.expect_err("TLS 1.1 unexpectedly reached SMTP authentication");
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("ProtocolVersion"), "{diagnostic}");
    assert!(!diagnostic.contains("smtp-tls11-credential"));
    assert!(!observed.contains("smtp-tls11-credential-must-not-be-written"));
    let observed_upper = observed.to_ascii_uppercase();
    assert!(!observed_upper.contains("AUTH "));
    assert!(!observed_upper.contains("MAIL FROM"));
    assert!(!observed_upper.contains("RCPT TO"));
    assert!(!observed_upper.contains("DATA\r\n"));
}
