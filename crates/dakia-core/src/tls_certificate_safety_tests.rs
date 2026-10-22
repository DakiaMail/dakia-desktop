//! Certificate-validation regressions for the production IMAP and SMTP
//! connectors.
//!
//! The DER fixtures were generated solely for this test suite with OpenSSL
//! 3.6.4. Serial numbers and validity windows are fixed. The private keys are
//! synthetic and have no use outside loopback tests. `TEST_ROOT` is valid from
//! 2020-01-01 through 2099-01-01. `EXPIRED_LEAF` expired on 2021-01-01.
//! `MISSING_CHAIN_LEAF` is valid through 2080-01-01 but is signed by
//! `TEST_INTERMEDIATE`; its tests deliberately omit that intermediate.

use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use lettre::address::Envelope;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::{timeout, Duration},
};
use tokio_rustls::{
    rustls::{
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        ServerConfig,
    },
    TlsAcceptor,
};

const TEST_ROOT_DER_BASE64: &str = "MIIBmTCCAT6gAwIBAgICA+kwCgYIKoZIzj0EAwIwIDEeMBwGA1UEAwwVRGFraWEtVExTLXNhZmV0eS1yb290MCAXDTIwMDEwMTAwMDAwMFoYDzIwOTkwMTAxMDAwMDAwWjAgMR4wHAYDVQQDDBVEYWtpYS1UTFMtc2FmZXR5LXJvb3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASTUsLJUHRjh0w7p32kxKNgeVamVyTlmFnClBqi9PEqYbPEDCNQugNDCIPRniT7OxCD0lYZ/hC7CQ0IniNHWizDo2YwZDASBgNVHRMBAf8ECDAGAQH/AgEBMA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQULeVSwzvTMlAPOEBdN8SM+IsYalkwHwYDVR0jBBgwFoAULeVSwzvTMlAPOEBdN8SM+IsYalkwCgYIKoZIzj0EAwIDSQAwRgIhAKtxM46u+SPnBI5nNvrtesu5Mb53yRM2qlnkFTWF6WLPAiEA5eVbXqY0cTuu42EAtIzEXHqtbGSJOVJ47ZJ/fZ0tWFQ=";
const TEST_INTERMEDIATE_DER_BASE64: &str = "MIIBoTCCAUagAwIBAgICA+owCgYIKoZIzj0EAwIwIDEeMBwGA1UEAwwVRGFraWEtVExTLXNhZmV0eS1yb290MCAXDTIwMDEwMTAwMDAwMFoYDzIwOTAwMTAxMDAwMDAwWjAoMSYwJAYDVQQDDB1EYWtpYS1UTFMtc2FmZXR5LWludGVybWVkaWF0ZTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABLae5w5JE0LQNZsI3So9pHOR1IRguNsUkywQJ25QhcmSsuqjpicrHXo1RFYth2a3z1YWqAGFtOEVvJ+0XKuq4jujZjBkMBIGA1UdEwEB/wQIMAYBAf8CAQAwDgYDVR0PAQH/BAQDAgEGMB0GA1UdDgQWBBTw7jaHxS4eZ2E5tHPOc8osf1chpDAfBgNVHSMEGDAWgBQt5VLDO9MyUA84QF03xIz4ixhqWTAKBggqhkjOPQQDAgNJADBGAiEAyqTM10t2EkQsnpv4akJ/DetEcDWCcRfPupgzh2MDf7ICIQDAsGqwFxB1Gr09amZwqf3BsHyU1Z/hxJoWqaNCra58gg==";
const EXPIRED_LEAF_DER_BASE64: &str = "MIIBtzCCAV2gAwIBAgICA+swCgYIKoZIzj0EAwIwIDEeMBwGA1UEAwwVRGFraWEtVExTLXNhZmV0eS1yb290MB4XDTIwMDEwMTAwMDAwMFoXDTIxMDEwMTAwMDAwMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEnIbQhZUolTDAe/t/6vQdD8jAvi7xZnzxEYkeipbVcBbTYgN32Otrfq3ZaAHbDkHHENEGqVKHCHaR5mO2i3DmzqOBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFFQnAYORe+PJeqhU3qVOiMDGXYo2MB8GA1UdIwQYMBaAFC3lUsM70zJQDzhAXTfEjPiLGGpZMAoGCCqGSM49BAMCA0gAMEUCIHdSk9FHs0codM93/Oghkjx1r+1MMrmSg3b04ZPVIN+3AiEAlMPx1O8x6As/ov7o8R0aj6832xyoCDAUz5ZJ6TSdYvI=";
const EXPIRED_LEAF_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgoMvPBsZJTjKMaMLYCzBM4D4fS0aYTqcRr7xyPIfS/BuhRANCAASchtCFlSiVMMB7+3/q9B0PyMC+LvFmfPERiR6KltVwFtNiA3fY62t+rdloAdsOQccQ0QapUocIdpHmY7aLcObO";
const MISSING_CHAIN_LEAF_DER_BASE64: &str = "MIIBwDCCAWegAwIBAgICA+wwCgYIKoZIzj0EAwIwKDEmMCQGA1UEAwwdRGFraWEtVExTLXNhZmV0eS1pbnRlcm1lZGlhdGUwIBcNMjAwMTAxMDAwMDAwWhgPMjA4MDAxMDEwMDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABHlqA6k/YTUTjSa6oxPrp57jbfIDb+obWKQbA0vYZ7AzdP8fwFukq9poHzJeLpYY6+NmIInXkRsxwetP+QgALpWjgZIwgY8wDAYDVR0TAQH/BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMB0GA1UdDgQWBBSdKefCfg9WS+/8VZBkkYn54YXh8jAfBgNVHSMEGDAWgBTw7jaHxS4eZ2E5tHPOc8osf1chpDAKBggqhkjOPQQDAgNHADBEAiB6RfAAS6j13oBZh9LDezw6Z95UWyXw6qp8We0dWAwyBwIgXXAwzt3HDEzULrkh5R109CwY8/vN8jUDcmrh+fJSS+Y=";
const MISSING_CHAIN_LEAF_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg3KA98pi4js8baH9XmyPPRUYxdwrm+5Ymp38RcThT1iGhRANCAAR5agOpP2E1E40muqMT66ee423yA2/qG1ikGwNL2GewM3T/H8BbpKvaaB8yXi6WGOvjZiCJ15EbMcHrT/kIAC6V";
// Independent synthetic root used only to prove that trusting one CA never
// trusts a chain issued by another. It is the public Dakia loopback SMTP test
// CA, valid from 2026-07-30 through 2036-07-27.
// Dedicated synthetic chain for the one-click sender. Its hostname is reserved
// and intentionally unresolvable, so a passing request proves reqwest used the
// sender's checked address pin rather than ambient DNS.
const FICTIONAL_UNSUBSCRIBE_ROOT_DER_BASE64: &str = "MIIBtzCCAV2gAwIBAgIUa9D2+G2mMh0/41ItdbJ9xp/ZC7UwCgYIKoZIzj0EAwIwMDEuMCwGA1UEAwwlRGFraWEgZmljdGlvbmFsIHVuc3Vic2NyaWJlIHRlc3Qgcm9vdDAgFw0yNjEwMTcyMTE0MTdaGA8yMDgxMDcyMDIxMTQxN1owMDEuMCwGA1UEAwwlRGFraWEgZmljdGlvbmFsIHVuc3Vic2NyaWJlIHRlc3Qgcm9vdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABMuNlirmSoLRHRLfeHv7ND4QB/Mj4+7hKr+eKDrMCAJwHwx7nryTFh9fkZK5ToAwqbbdlCfM7TFFn8mFx8S0mb+jUzBRMB0GA1UdDgQWBBQ8+YIdZWHx3T8KpxvzxkATekpogDAfBgNVHSMEGDAWgBQ8+YIdZWHx3T8KpxvzxkATekpogDAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIQDnENy8meMcgo6XDWDbP6OGDW4dqRvBWaTrbIoSNGGmCQIgYC4j74x3XzReRIJuYbqdgBDj3ZJqAu6tiW7m4LOg/vQ=";
const FICTIONAL_UNSUBSCRIBE_LEAF_DER_BASE64: &str = "MIIBzjCCAXSgAwIBAgIUCPClqO3l7OYUolescrFrN2e3b9kwCgYIKoZIzj0EAwIwMDEuMCwGA1UEAwwlRGFraWEgZmljdGlvbmFsIHVuc3Vic2NyaWJlIHRlc3Qgcm9vdDAgFw0yNjEwMTcyMTE0MTdaGA8yMDU0MDMwNDIxMTQxN1owKzEpMCcGA1UEAwwgdW5yZXNvbHZhYmxlLXVuc3Vic2NyaWJlLmludmFsaWQwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARNL5g6M9ba+y6QIuPNhu4rzdxq0oYXk4xb0BRAYFoWV48IyhOBhOxmF5+n76mTTntf17VQp0BMgA9ktLzVs4ceo28wbTArBgNVHREEJDAigiB1bnJlc29sdmFibGUtdW5zdWJzY3JpYmUuaW52YWxpZDAdBgNVHQ4EFgQUxYOgJjPTvd+ZFUT3Sw2On002HhowHwYDVR0jBBgwFoAUPPmCHWVh8d0/Cqcb88ZAE3pKaIAwCgYIKoZIzj0EAwIDSAAwRQIge3pdRcHANcjgI7SigaH4JD9x7e0W3261Yg2hoTu4UocCIQD6ha5/wZ17zbsA9KvI5npUd8sBd0Dsdev7g6tmf0Ottg==";
const FICTIONAL_UNSUBSCRIBE_LEAF_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgA0MRhi+fmDRbCk9Dc4GWyNnHuGIgR80w8NyH+/8t47yhRANCAARNL5g6M9ba+y6QIuPNhu4rzdxq0oYXk4xb0BRAYFoWV48IyhOBhOxmF5+n76mTTntf17VQp0BMgA9ktLzVs4ce";

const UNRELATED_ROOT_DER_BASE64: &str = "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";

fn certificate_test_account(port: u16) -> Account {
    Account {
        id: uuid::Uuid::new_v4(),
        email: "sender@example.test".into(),
        account_name: "TLS certificate fixture".into(),
        display_name: "TLS fixture".into(),
        provider_id: "fixture".into(),
        auth: AccountAuth::Password {
            username: "sender@example.test".into(),
        },
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        imap_security: Security::Tls,
        smtp_host: "127.0.0.1".into(),
        smtp_port: port,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    }
}

fn certificate_test_envelope() -> Envelope {
    Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap()
}

pub(super) fn fixture_acceptor(
    leaf_base64: &str,
    key_base64: &str,
    intermediates: &[&str],
) -> TlsAcceptor {
    let mut chain = vec![CertificateDer::from(STANDARD.decode(leaf_base64).unwrap())];
    chain.extend(
        intermediates
            .iter()
            .map(|certificate| CertificateDer::from(STANDARD.decode(certificate).unwrap())),
    );
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        STANDARD.decode(key_base64).unwrap(),
    ));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

/// A reserved DNS name paired with a synthetic trusted chain. The caller must
/// pin it to a controlled address with `resolve_to_addrs`; it has no DNS record.
pub(super) fn trusted_fictional_unsubscribe_tls_fixture() -> (TlsAcceptor, Vec<u8>) {
    (
        fixture_acceptor(
            FICTIONAL_UNSUBSCRIBE_LEAF_DER_BASE64,
            FICTIONAL_UNSUBSCRIBE_LEAF_KEY_DER_BASE64,
            &[],
        ),
        STANDARD
            .decode(FICTIONAL_UNSUBSCRIBE_ROOT_DER_BASE64)
            .unwrap(),
    )
}

#[derive(Debug)]
enum TlsServerObservation {
    PeerRejected,
    ApplicationData(Vec<u8>),
    SilentAfterGreeting,
}

async fn rejected_tls_server(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    protocol_greeting: &'static [u8],
) -> TlsServerObservation {
    let (stream, _) = listener.accept().await.unwrap();
    let Ok(mut stream) = acceptor.accept(stream).await else {
        return TlsServerObservation::PeerRejected;
    };
    // A server that simply closes after TLS can make a disabled certificate
    // verifier look like a certificate rejection. Send a valid application
    // greeting after any accepted handshake. A client that accepts the bad
    // certificate must now either return success (IMAP) or send EHLO (SMTP),
    // both of which make the regression fail.
    if stream.write_all(protocol_greeting).await.is_err() || stream.flush().await.is_err() {
        return TlsServerObservation::PeerRejected;
    }
    let mut observed = vec![0; 4096];
    match timeout(Duration::from_millis(500), stream.read(&mut observed)).await {
        Ok(Ok(0)) | Ok(Err(_)) => TlsServerObservation::PeerRejected,
        Ok(Ok(length)) => {
            observed.truncate(length);
            TlsServerObservation::ApplicationData(observed)
        }
        Err(_) => TlsServerObservation::SilentAfterGreeting,
    }
}

async fn imap_certificate_rejection(
    acceptor: TlsAcceptor,
    trusted_root_base64: &str,
) -> (anyhow::Error, TlsServerObservation) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(rejected_tls_server(
        listener,
        acceptor,
        b"* OK synthetic IMAP server ready\r\n",
    ));
    let account = certificate_test_account(port);
    let endpoint = MailService::imap_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        "127.0.0.1".into(),
        STANDARD.decode(trusted_root_base64).unwrap(),
    )
    .unwrap();
    let error = match ImapClient::connect_with_endpoint(&account, endpoint).await {
        Ok(_) => panic!("invalid certificate unexpectedly established IMAP TLS"),
        Err(error) => error,
    };
    (error, server.await.unwrap())
}

async fn smtp_certificate_rejection(
    acceptor: TlsAcceptor,
    trusted_root_base64: &str,
    server_name: &str,
) -> (anyhow::Error, TlsServerObservation) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(rejected_tls_server(
        listener,
        acceptor,
        b"220 synthetic SMTP server ready\r\n",
    ));
    let account = certificate_test_account(port);
    let endpoint = MailService::smtp_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        server_name.into(),
        STANDARD.decode(trusted_root_base64).unwrap(),
    )
    .unwrap();
    let error = send_smtp_raw(
        &account,
        &certificate_test_envelope(),
        b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: certificate boundary\r\n\r\nbody\r\n",
        "synthetic-secret-must-not-leak",
        endpoint,
        Duration::from_secs(2),
    )
    .await
    .expect_err("invalid certificate unexpectedly reached SMTP");
    (error, server.await.unwrap())
}

fn assert_peer_rejected_before_protocol(observed: TlsServerObservation, protocol: &str) {
    match observed {
        TlsServerObservation::PeerRejected => {}
        TlsServerObservation::ApplicationData(bytes) => panic!(
            "{protocol} sent application data after accepting an invalid certificate: {:?}",
            String::from_utf8_lossy(&bytes)
        ),
        TlsServerObservation::SilentAfterGreeting => panic!(
            "{protocol} left TLS open after the server greeting instead of rejecting the certificate"
        ),
    }
}

fn assert_imap_tls_rejection(error: &anyhow::Error, observed: TlsServerObservation) {
    assert!(
        error.to_string().contains("TLS handshake failed"),
        "{error:#}"
    );
    assert!(!format!("{error:#}").contains("synthetic-secret"));
    assert_peer_rejected_before_protocol(observed, "IMAP");
}

fn assert_safe_smtp_tls_rejection(error: &anyhow::Error, observed: TlsServerObservation) {
    let failure = error
        .downcast_ref::<SendFailure>()
        .expect("typed send failure");
    assert_eq!(failure.kind, SendFailureKind::NotSubmitted);
    assert_eq!(
        failure.message,
        "Dakia could not submit this message. It was not sent."
    );
    assert!(!failure.message.contains("certificate"));
    assert!(!failure.message.contains("synthetic-secret"));
    assert_peer_rejected_before_protocol(observed, "SMTP");
}

#[tokio::test]
async fn valid_complete_chain_reaches_imap_and_smtp_authentication() {
    let imap_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imap_port = imap_listener.local_addr().unwrap().port();
    let imap_acceptor = fixture_acceptor(
        MISSING_CHAIN_LEAF_DER_BASE64,
        MISSING_CHAIN_LEAF_KEY_DER_BASE64,
        &[TEST_INTERMEDIATE_DER_BASE64],
    );
    let imap_server = tokio::spawn(async move {
        let (stream, _) = imap_listener.accept().await.unwrap();
        let stream = imap_acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        connection
            .get_mut()
            .write_all(b"* OK synthetic IMAP server ready\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();
        let mut command = String::new();
        connection.read_line(&mut command).await.unwrap();
        assert_eq!(command, "D0001 CAPABILITY\r\n");
        connection
            .get_mut()
            .write_all(b"* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();
        command.clear();
        connection.read_line(&mut command).await.unwrap();
        assert_eq!(
            command,
            "D0002 LOGIN \"sender@example.test\" \"synthetic-secret\"\r\n"
        );
        connection
            .get_mut()
            .write_all(b"D0002 OK authenticated\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();
    });
    let imap_account = certificate_test_account(imap_port);
    let imap_endpoint = MailService::imap_loopback_endpoint(
        "127.0.0.1".into(),
        imap_port,
        "127.0.0.1".into(),
        STANDARD.decode(TEST_ROOT_DER_BASE64).unwrap(),
    )
    .unwrap();
    let mut imap_client = ImapClient::connect_with_endpoint(&imap_account, imap_endpoint)
        .await
        .unwrap();
    imap_client
        .authenticate(&imap_account, "synthetic-secret")
        .await
        .unwrap();
    imap_server.await.unwrap();

    let smtp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp_port = smtp_listener.local_addr().unwrap().port();
    let smtp_acceptor = fixture_acceptor(
        MISSING_CHAIN_LEAF_DER_BASE64,
        MISSING_CHAIN_LEAF_KEY_DER_BASE64,
        &[TEST_INTERMEDIATE_DER_BASE64],
    );
    let smtp_server = tokio::spawn(async move {
        let (stream, _) = smtp_listener.accept().await.unwrap();
        let stream = smtp_acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        connection
            .get_mut()
            .write_all(b"220 synthetic SMTP server ready\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();

        let mut command = String::new();
        connection.read_line(&mut command).await.unwrap();
        assert!(
            command.starts_with("EHLO "),
            "unexpected SMTP command: {command:?}"
        );
        connection
            .get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();

        command.clear();
        connection.read_line(&mut command).await.unwrap();
        assert!(
            command.starts_with("EHLO "),
            "unexpected SMTP command: {command:?}"
        );
        connection
            .get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();

        command.clear();
        connection.read_line(&mut command).await.unwrap();
        assert!(
            command.starts_with("AUTH PLAIN "),
            "unexpected SMTP command: {command:?}"
        );
        connection
            .get_mut()
            .write_all(b"235 2.7.0 authenticated\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();

        command.clear();
        connection.read_line(&mut command).await.unwrap();
        assert_eq!(command, "QUIT\r\n");
        connection
            .get_mut()
            .write_all(b"221 2.0.0 bye\r\n")
            .await
            .unwrap();
        connection.get_mut().flush().await.unwrap();
    });
    let smtp_account = certificate_test_account(smtp_port);
    let smtp_endpoint = MailService::smtp_loopback_endpoint(
        "127.0.0.1".into(),
        smtp_port,
        "127.0.0.1".into(),
        STANDARD.decode(TEST_ROOT_DER_BASE64).unwrap(),
    )
    .unwrap();
    smtp_auth_probe_with_smtp_endpoint(
        &smtp_account,
        "synthetic-secret",
        smtp_endpoint,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    smtp_server.await.unwrap();
}

#[tokio::test]
async fn imap_rejects_expired_server_certificate_before_login() {
    let (error, observed) = imap_certificate_rejection(
        fixture_acceptor(EXPIRED_LEAF_DER_BASE64, EXPIRED_LEAF_KEY_DER_BASE64, &[]),
        TEST_ROOT_DER_BASE64,
    )
    .await;
    assert_imap_tls_rejection(&error, observed);
}

#[tokio::test]
async fn imap_rejects_missing_intermediate_before_login() {
    let (error, observed) = imap_certificate_rejection(
        fixture_acceptor(
            MISSING_CHAIN_LEAF_DER_BASE64,
            MISSING_CHAIN_LEAF_KEY_DER_BASE64,
            &[],
        ),
        TEST_ROOT_DER_BASE64,
    )
    .await;
    assert_imap_tls_rejection(&error, observed);
}

#[tokio::test]
async fn smtp_rejects_expired_server_certificate_before_auth_or_envelope() {
    let (error, observed) = smtp_certificate_rejection(
        fixture_acceptor(EXPIRED_LEAF_DER_BASE64, EXPIRED_LEAF_KEY_DER_BASE64, &[]),
        TEST_ROOT_DER_BASE64,
        "127.0.0.1",
    )
    .await;
    assert_safe_smtp_tls_rejection(&error, observed);
}

#[tokio::test]
async fn smtp_rejects_missing_intermediate_before_auth_or_envelope() {
    let (error, observed) = smtp_certificate_rejection(
        fixture_acceptor(
            MISSING_CHAIN_LEAF_DER_BASE64,
            MISSING_CHAIN_LEAF_KEY_DER_BASE64,
            &[],
        ),
        TEST_ROOT_DER_BASE64,
        "127.0.0.1",
    )
    .await;
    assert_safe_smtp_tls_rejection(&error, observed);
}

#[tokio::test]
async fn smtp_rejects_hostname_mismatch_before_auth_or_envelope() {
    let (error, observed) = smtp_certificate_rejection(
        fixture_acceptor(
            MISSING_CHAIN_LEAF_DER_BASE64,
            MISSING_CHAIN_LEAF_KEY_DER_BASE64,
            &[TEST_INTERMEDIATE_DER_BASE64],
        ),
        TEST_ROOT_DER_BASE64,
        "mail.invalid",
    )
    .await;
    assert_safe_smtp_tls_rejection(&error, observed);
}

#[tokio::test]
async fn smtp_rejects_untrusted_issuer_before_auth_or_envelope() {
    let (error, observed) = smtp_certificate_rejection(
        fixture_acceptor(
            MISSING_CHAIN_LEAF_DER_BASE64,
            MISSING_CHAIN_LEAF_KEY_DER_BASE64,
            &[TEST_INTERMEDIATE_DER_BASE64],
        ),
        UNRELATED_ROOT_DER_BASE64,
        "127.0.0.1",
    )
    .await;
    assert_safe_smtp_tls_rejection(&error, observed);
}
