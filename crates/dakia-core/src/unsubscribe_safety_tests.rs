//! Cryptographic unsubscribe fixtures.
//!
//! The private key is generated solely for this test module.  DNS is supplied
//! through the mail-auth resolver cache, so these tests never depend on a
//! public DNS record or an external provider.

use super::*;
use crate::{provider, AccountDraft};
use async_trait::async_trait;
use mail_auth::{
    common::{
        crypto::{RsaKey, Sha256},
        parse::TxtRecordParser,
        verify::DomainKey,
    },
    dkim::DkimSigner,
    ResolverCache, Txt,
};
use std::{
    borrow::Borrow,
    collections::{HashMap, VecDeque},
    hash::Hash,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    time::timeout,
};
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs1KeyDer};

const PRIVATE_KEY_DER_BASE64: &str = "MIIEowIBAAKCAQEA3I6SFyvPtIu3cDRNzeV1FqxH0F4Of3A2VVB/0UMCDb+z0Z8U7s7Na3gTOk4pFvtzaqTXSLgaYzh0/eLSgsbZz67xnvlGazkH12UjK3NuMbmx3IW0mviNRhqC8iHeoCmgzsLKNreaFkureQt3bhdh95YyE8fr6XsFUM1FtGBLaJlP61mPBb1Th+SCpQGtml1riefiRaRSnwsXfj+c14SBxLZqj62ZHASquvV7+bKtCDeiRfzwMXJelQu2L14QXICvPo4k2Oin6s+oT61D7I4FlYo8rC/7Z82F+BV61osNokLTobS5RuIgZUqXUk00dbazen5g5X0Xa01WYm67JC6kVQIDAQABAoIBACoOobr7ZrsF3W/u+VsmwyIUrgkEtn0El8fC9uyfe3fFs3Nog2RaWP+CQSBgISO5xTNzxX2fZaC/9kcnhn7bjlM1B+wJmqz9jgo7xe4WL9AZmdn0QRFADihtC8VCQiKGbZyftlGixyYzPbpyPk7m9I5yvI/bEphOFiBnVEBmEFscbkC/0ZIkGg1TV3xhKIld66bFbry1Bn6nJ4z3el0e6kkovUYQRuecYthD6BP6JId5iNmJ72m3LQpN+mvG31xh6XLuwG5Nt5BY/2bpM/AXmOQZRTkUFzer7WHDJTTwDHDZE6ghH0+hE33HHvYB7WVXeUawQLYTqQaefhUoxPP+Z00CgYEA/FKG/ishf/GaYVZJMf5U6zn9vkeGeRVi6JGr/LCN7MFB7r+6+t7LQJMB6CVCAN3DVoTytmiNhHJ+jFaG8SyX+O4x2RmNmoxigLClGRuqc7/+ToAM1n8PiaJIBoP/j+cqDNHQDL7WK7dwNi6PLa5wG+HJiI+yFm14eFj9ukc0ygMCgYEA38WE5qVFzm/FFZ3HesJE6S5G0w3K3HlBd77ajt8DtF/XartenYEF/PqFYc7dG/r4DFoL0xSdWCC6VpyRU69MlF+jXwCOXqGVvZKbDBUiO74G0BKrCPj71BvxPf6nZ4G/t+UURZmNqe7B2AgMiXI5nNEcbdkIQG12tEu2kV5fNMcCgYBw7DZB9eeOLQBcY5Nqblpu04s95yDFe87CKr9/3TwHIw5w3Rm/QIpSIfiOfm4lmRnc1K8S94OuLNd50xedQ8nXvDJO6OK4se87jNK2g+0q1FfppeEY9dBsVlZpDVxn5DwH84uhhGviu8LMJcfaJv/vnLFTRn0c5WgtqMHBOsZJQKBgQCDFHsTeMD5enOig/g0KtSfG2KVg0DQAZ+d+4ifpHfRE0tm1DdKYCgX1tCHSNww5cSjx6OhlDPPSeydi719nfRn/dX/cp3zRQgoMlbsrvyMrnXjMOxLpz4tM+kO/bsmYNTjbid/1cEsSBX47E4bvXOOLthOaJHy26xrVOavJO70RQKBgFKGLx+YUASQcbR9BSyC0UDi58WSlOLTw970ykzmhOVG1xx7bAAlz3BEKPxIGDpEjw7rf18+m03XTebPmoVTyXWk38BmOUkxUWkZd9+c/dsfnV8UXwDUYLDzZ1nTXB3S022LX0hO+HxgiwuQ6z9+s1IHp70NykXcl0mZxsIEqsr2";
const PUBLIC_KEY_PKCS1_BASE64: &str = "MIIBCgKCAQEA3I6SFyvPtIu3cDRNzeV1FqxH0F4Of3A2VVB/0UMCDb+z0Z8U7s7Na3gTOk4pFvtzaqTXSLgaYzh0/eLSgsbZz67xnvlGazkH12UjK3NuMbmx3IW0mviNRhqC8iHeoCmgzsLKNreaFkureQt3bhdh95YyE8fr6XsFUM1FtGBLaJlP61mPBb1Th+SCpQGtml1riefiRaRSnwsXfj+c14SBxLZqj62ZHASquvV7+bKtCDeiRfzwMXJelQu2L14QXICvPo4k2Oin6s+oT61D7I4FlYo8rC/7Z82F+BV61osNokLTobS5RuIgZUqXUk00dbazen5g5X0Xa01WYm67JC6kVQIDAQAB";

#[derive(Default)]
struct TxtCache(Mutex<HashMap<Box<str>, Txt>>);

impl ResolverCache<Box<str>, Txt> for TxtCache {
    fn get<Q>(&self, key: &Q) -> Option<Txt>
    where
        Box<str>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let value = self.0.lock().unwrap().get(key).cloned();
        value
    }

    fn remove<Q>(&self, key: &Q) -> Option<Txt>
    where
        Box<str>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.0.lock().unwrap().remove(key)
    }

    fn insert(&self, key: Box<str>, value: Txt, _: Instant) {
        self.0.lock().unwrap().insert(key, value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedOneClickRequest {
    url: String,
    host: String,
    addresses: Vec<SocketAddr>,
    content_type: String,
    body: String,
}

struct RecordingOneClickSender {
    status: u16,
    requests: Mutex<Vec<RecordedOneClickRequest>>,
}

impl RecordingOneClickSender {
    fn new(status: u16) -> Self {
        Self {
            status,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<RecordedOneClickRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl OneClickSender for RecordingOneClickSender {
    async fn post(&self, request: OneClickRequest<'_>) -> Result<u16> {
        self.requests.lock().unwrap().push(RecordedOneClickRequest {
            url: request.url.to_string(),
            host: request.host.to_owned(),
            addresses: request.addresses.to_vec(),
            content_type: request.content_type.to_owned(),
            body: request.body.to_owned(),
        });
        Ok(self.status)
    }
}

enum ScriptedResolverOutcome {
    Addresses(Vec<SocketAddr>),
    Error(&'static str),
    Pending,
}

struct ScriptedOneClickResolver {
    calls: AtomicUsize,
    queries: Mutex<Vec<(String, u16)>>,
    outcomes: Mutex<VecDeque<ScriptedResolverOutcome>>,
}

impl ScriptedOneClickResolver {
    fn new(outcomes: impl IntoIterator<Item = ScriptedResolverOutcome>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            queries: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes.into_iter().collect()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn queries(&self) -> Vec<(String, u16)> {
        self.queries.lock().unwrap().clone()
    }
}

#[async_trait]
impl OneClickResolver for ScriptedOneClickResolver {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.queries.lock().unwrap().push((host.to_owned(), port));
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("resolver must have a scripted outcome");
        match outcome {
            ScriptedResolverOutcome::Addresses(addresses) => Ok(addresses),
            ScriptedResolverOutcome::Error(message) => Err(anyhow::anyhow!(message)),
            ScriptedResolverOutcome::Pending => std::future::pending().await,
        }
    }
}

fn signed_message(headers: &[&str]) -> Vec<u8> {
    let raw = concat!(
        "From: list@example.test\r\n",
        "To: recipient@example.test\r\n",
        "List-Unsubscribe: <https://unsubscribe.example.test/token>\r\n",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
        "Subject: safety fixture\r\n",
        "Date: Wed, 26 Aug 2026 12:00:00 +0000\r\n",
        "\r\n",
        "unsubscribe body\r\n"
    );
    let key_der = base64::engine::general_purpose::STANDARD
        .decode(PRIVATE_KEY_DER_BASE64.replace("VlZpDV", "VlZJpDV"))
        .unwrap();
    let key =
        RsaKey::<Sha256>::from_key_der(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key_der)))
            .unwrap();
    let signature = DkimSigner::from_key(key)
        .domain("example.test")
        .selector("safety")
        .headers(headers.iter().copied())
        .header_canonicalization(mail_auth::dkim::Canonicalization::Simple)
        .body_canonicalization(mail_auth::dkim::Canonicalization::Simple)
        .sign(raw.as_bytes())
        .unwrap();
    let mut signed = Vec::with_capacity(raw.len() + 512);
    signature.write(&mut signed, true);
    signed.extend_from_slice(raw.as_bytes());
    signed
}

fn authenticator_and_cache() -> (MessageAuthenticator, TxtCache) {
    let public_record = format!("v=DKIM1; k=rsa; p={PUBLIC_KEY_PKCS1_BASE64}");
    let key = DomainKey::parse(public_record.as_bytes()).unwrap();
    let cache = TxtCache::default();
    cache.insert(
        "safety._domainkey.example.test.".into(),
        Txt::DomainKey(std::sync::Arc::new(key)),
        Instant::now() + Duration::from_secs(60),
    );
    (
        MessageAuthenticator::new(
            mail_auth::hickory_resolver::config::ResolverConfig::default(),
            mail_auth::hickory_resolver::config::ResolverOpts::default(),
        )
        .unwrap(),
        cache,
    )
}

/// Drive the same `MessageAuthenticator` call used by `parse_message` through
/// a fictional local DNS authority. The existing TXT-cache fixture is ideal
/// for direct verifier tests, but `parse_message` deliberately owns only an
/// authenticator, not a caller-supplied cache.
async fn parse_message_authenticator() -> (MessageAuthenticator, tokio::task::JoinHandle<()>) {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    let public_record = format!("v=DKIM1; k=rsa; p={PUBLIC_KEY_PKCS1_BASE64}");
    let task = tokio::spawn(async move {
        let mut query = [0_u8; 2048];
        let (length, peer) = timeout(Duration::from_secs(1), server.recv_from(&mut query))
            .await
            .expect("parse_message must query the fictional DKIM authority")
            .unwrap();
        assert_eq!(
            dns_question_name(&query[..length]),
            "safety._domainkey.example.test."
        );
        let response = dns_txt_response(&query[..length], public_record.as_bytes());
        server.send_to(&response, peer).await.unwrap();
    });

    let mut name_server =
        mail_auth::hickory_resolver::config::NameServerConfig::udp(Ipv4Addr::LOCALHOST.into());
    name_server.connections[0].port = port;
    let mut options = mail_auth::hickory_resolver::config::ResolverOpts::default();
    options.timeout = Duration::from_secs(1);
    options.attempts = 1;
    options.edns0 = false;
    options.num_concurrent_reqs = 1;
    (
        MessageAuthenticator::new(
            mail_auth::hickory_resolver::config::ResolverConfig::from_parts(
                None,
                vec![],
                vec![name_server],
            ),
            options,
        )
        .unwrap(),
        task,
    )
}

fn dns_question_name(query: &[u8]) -> String {
    assert!(query.len() >= 17, "DNS query must include a question");
    let mut cursor = 12;
    let mut labels = Vec::new();
    while query[cursor] != 0 {
        let length = usize::from(query[cursor]);
        cursor += 1;
        assert!(
            length > 0 && cursor + length <= query.len(),
            "DNS query contains an invalid label"
        );
        labels.push(std::str::from_utf8(&query[cursor..cursor + length]).unwrap());
        cursor += length;
    }
    assert!(cursor + 5 <= query.len(), "DNS question is incomplete");
    format!("{}.", labels.join("."))
}

fn dns_txt_response(query: &[u8], value: &[u8]) -> Vec<u8> {
    let mut question_end = 12;
    while query[question_end] != 0 {
        question_end += usize::from(query[question_end]) + 1;
    }
    question_end += 5;

    let chunks = value.chunks(u8::MAX.into()).collect::<Vec<_>>();
    let rdata_length = chunks.iter().map(|chunk| chunk.len() + 1).sum::<usize>();
    let mut response = Vec::with_capacity(question_end + rdata_length + 12);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    response.extend_from_slice(&[0xc0, 0x0c, 0, 16, 0, 1, 0, 0, 0, 60]);
    response.extend_from_slice(&(rdata_length as u16).to_be_bytes());
    for chunk in chunks {
        response.push(chunk.len() as u8);
        response.extend_from_slice(chunk);
    }
    response
}

fn unsubscribe_test_account() -> Account {
    AccountDraft {
        email: "recipient@example.test".into(),
        display_name: "Fictional recipient".into(),
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
    .into_account(provider::by_id("fastmail").unwrap())
}

#[tokio::test]
async fn signed_rfc8058_headers_pass_and_tampering_fails() {
    let signed = signed_message(&[
        "From",
        "List-Unsubscribe",
        "List-Unsubscribe-Post",
        "Subject",
    ]);
    let (authenticator, cache) = authenticator_and_cache();
    assert!(dkim_covers_unsubscribe_headers_with_txt_cache(&signed, &authenticator, &cache,).await);
    let parsed = parse_complete_message(&signed).unwrap();
    assert_eq!(
        extract_unsubscribe_with_txt_cache(&parsed, &signed, &authenticator, &cache).await,
        (
            Some("one_click".into()),
            Some("https://unsubscribe.example.test/token".into())
        )
    );

    let tampered = String::from_utf8(signed)
        .unwrap()
        .replace("/token>", "/tampered>")
        .into_bytes();
    assert!(
        !dkim_covers_unsubscribe_headers_with_txt_cache(&tampered, &authenticator, &cache,).await
    );
}

#[tokio::test]
async fn signed_message_that_omits_either_required_header_cannot_enable_one_click() {
    for headers in [
        &["From", "List-Unsubscribe"][..],
        &["From", "List-Unsubscribe-Post"][..],
    ] {
        let signed = signed_message(headers);
        let (authenticator, cache) = authenticator_and_cache();
        assert!(
            !dkim_covers_unsubscribe_headers_with_txt_cache(&signed, &authenticator, &cache,).await
        );
    }
}

#[tokio::test]
async fn duplicate_rfc8058_headers_fall_back_without_selecting_one_click() {
    let raw = concat!(
        "List-Unsubscribe: <https://unsubscribe.example.test/first>\r\n",
        "List-Unsubscribe: <https://unsubscribe.example.test/second>\r\n",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
        "\r\n",
        "body"
    );
    let parsed = parse_complete_message(raw.as_bytes()).unwrap();
    assert_eq!(
        extract_unsubscribe(&parsed, raw.as_bytes(), None).await,
        (
            Some("web".into()),
            Some("https://unsubscribe.example.test/first".into())
        )
    );
}

#[tokio::test]
async fn signed_one_click_authorization_reaches_the_pinned_post_pipeline_with_only_rfc8058_data() {
    let signed = signed_message(&[
        "From",
        "List-Unsubscribe",
        "List-Unsubscribe-Post",
        "Subject",
    ]);
    let (authenticator, cache) = authenticator_and_cache();
    assert!(
        dkim_covers_unsubscribe_headers_with_txt_cache(&signed, &authenticator, &cache).await,
        "the request pipeline must never be enabled by an unsigned one-click header"
    );

    let url = Url::parse("https://unsubscribe.example.test/token?recipient=opaque").unwrap();
    let (host, _) = one_click_target(&url).unwrap();
    let addresses =
        public_unsubscribe_addresses([SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)])
            .unwrap();
    let sender = RecordingOneClickSender::new(204);

    post_one_click_unsubscribe_with_sender(&url, host, &addresses, &sender)
        .await
        .unwrap();

    assert_eq!(
        sender.requests(),
        vec![RecordedOneClickRequest {
            url: url.to_string(),
            host: "unsubscribe.example.test".into(),
            addresses,
            content_type: "application/x-www-form-urlencoded".into(),
            body: "List-Unsubscribe=One-Click".into(),
        }],
        "only a POST with the RFC 8058 form payload may leave Dakia; the resolver result is pinned into the request"
    );
}

#[tokio::test]
async fn signed_raw_message_persists_and_only_its_explicit_action_reaches_the_pinned_post() {
    let raw = signed_message(&[
        "From",
        "List-Unsubscribe",
        "List-Unsubscribe-Post",
        "Subject",
    ]);
    let account = unsubscribe_test_account();
    let (authenticator, dns_server) = parse_message_authenticator().await;
    let message = parse_message(&account, "INBOX", 42, &[], &raw, Some(&authenticator))
        .await
        .expect("the signed raw message must take the production MIME parse path");
    timeout(Duration::from_secs(2), dns_server)
        .await
        .expect("fictional DKIM authority must finish")
        .unwrap();
    assert_eq!(message.unsubscribe_kind.as_deref(), Some("one_click"));
    assert_eq!(
        message.unsubscribe_url.as_deref(),
        Some("https://unsubscribe.example.test/token")
    );

    let store = Store::in_memory().await.unwrap();
    store.save_account(&account).await.unwrap();
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .unwrap();
    let persisted = store.message(&message.id).await.unwrap().unwrap();
    assert_eq!(persisted.unsubscribe_kind.as_deref(), Some("one_click"));
    assert_eq!(
        persisted.unsubscribe_url.as_deref(),
        Some("https://unsubscribe.example.test/token")
    );

    let resolver =
        ScriptedOneClickResolver::new([ScriptedResolverOutcome::Addresses(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            443,
        )])]);
    let sender = RecordingOneClickSender::new(204);
    assert_eq!(
        resolver.calls(),
        0,
        "parsing and persistence must not resolve"
    );
    assert!(
        sender.requests().is_empty(),
        "parsing and persistence must not submit an unsubscribe request"
    );
    let outcome = MailService::new(store)
        .unsubscribe_with_one_click_transport(
            &persisted,
            &resolver,
            Duration::from_secs(1),
            &sender,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, UnsubscribeOutcome::Completed));
    assert_eq!(resolver.calls(), 1);
    assert_eq!(
        sender.requests(),
        vec![RecordedOneClickRequest {
            url: "https://unsubscribe.example.test/token".into(),
            host: "unsubscribe.example.test".into(),
            addresses: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)],
            content_type: "application/x-www-form-urlencoded".into(),
            body: "List-Unsubscribe=One-Click".into(),
        }]
    );
}

#[tokio::test]
async fn tampered_or_unsigned_raw_unsubscribe_headers_are_persisted_as_web_and_never_posted() {
    let signed = signed_message(&[
        "From",
        "List-Unsubscribe",
        "List-Unsubscribe-Post",
        "Subject",
    ]);
    let cases = [
        String::from_utf8(signed.clone())
            .unwrap()
            .replace("/token>", "/tampered>")
            .into_bytes(),
        signed_message(&["From", "List-Unsubscribe", "Subject"]),
    ];

    for raw in cases {
        let account = unsubscribe_test_account();
        let (authenticator, dns_server) = parse_message_authenticator().await;
        let message = parse_message(&account, "INBOX", 43, &[], &raw, Some(&authenticator))
            .await
            .unwrap();
        timeout(Duration::from_secs(2), dns_server)
            .await
            .expect("fictional DKIM authority must finish")
            .unwrap();
        assert_eq!(message.unsubscribe_kind.as_deref(), Some("web"));
        let store = Store::in_memory().await.unwrap();
        store.save_account(&account).await.unwrap();
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .unwrap();
        let persisted = store.message(&message.id).await.unwrap().unwrap();

        let resolver = ScriptedOneClickResolver::new([]);
        let sender = RecordingOneClickSender::new(204);
        let outcome = MailService::new(store)
            .unsubscribe_with_one_click_transport(
                &persisted,
                &resolver,
                Duration::from_secs(1),
                &sender,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, UnsubscribeOutcome::Web(_)));
        assert_eq!(resolver.calls(), 0);
        assert!(sender.requests().is_empty());
    }
}

#[test]
fn one_click_destination_rejects_unsafe_schemes_credentials_and_private_or_mixed_answers() {
    for raw in [
        "http://unsubscribe.example.test/token",
        "file:///private/token",
        "https://user:password@unsubscribe.example.test/token",
        "https://localhost/token",
        "https://127.0.0.1/token",
        "https://[::1]/token",
    ] {
        assert!(
            one_click_target(&Url::parse(raw).unwrap()).is_err(),
            "unsafe destination unexpectedly passed: {raw}"
        );
    }

    let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443);
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443);
    let private = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 443);
    for answers in [vec![], vec![loopback], vec![private], vec![public, private]] {
        assert!(
            public_unsubscribe_addresses(answers).is_err(),
            "a private or mixed DNS answer must fail closed before the sender receives any address"
        );
    }
}

#[tokio::test]
async fn one_click_uses_one_public_resolution_and_pins_that_answer_set() {
    let url = Url::parse("https://unsubscribe.example.test:8443/token").unwrap();
    let initial_answers = vec![
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 8443),
        SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 8443),
    ];
    let resolver = ScriptedOneClickResolver::new([
        ScriptedResolverOutcome::Addresses(initial_answers.clone()),
        // A rebinding response must be irrelevant because the request uses
        // the first vetted answer set rather than resolving a second time.
        ScriptedResolverOutcome::Addresses(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            8443,
        )]),
    ]);
    let sender = RecordingOneClickSender::new(204);

    post_one_click_unsubscribe_with_resolver(&url, &resolver, Duration::from_secs(1), &sender)
        .await
        .unwrap();

    assert_eq!(
        resolver.calls(),
        1,
        "one-click may resolve its host only once"
    );
    assert_eq!(
        resolver.queries(),
        vec![("unsubscribe.example.test".into(), 8443)],
        "the resolver must receive the non-default URL port"
    );
    assert_eq!(
        sender.requests(),
        vec![RecordedOneClickRequest {
            url: url.to_string(),
            host: "unsubscribe.example.test".into(),
            addresses: initial_answers,
            content_type: "application/x-www-form-urlencoded".into(),
            body: "List-Unsubscribe=One-Click".into(),
        }],
        "the vetted answer set must be pinned into the one-click request"
    );
}

#[tokio::test]
async fn one_click_empty_answers_never_reach_the_sender() {
    let url = Url::parse("https://unsubscribe.example.test/token").unwrap();
    let resolver = ScriptedOneClickResolver::new([ScriptedResolverOutcome::Addresses(Vec::new())]);
    let sender = RecordingOneClickSender::new(204);

    assert!(
        post_one_click_unsubscribe_with_resolver(&url, &resolver, Duration::from_secs(1), &sender,)
            .await
            .is_err(),
        "an empty DNS response must fail closed"
    );
    assert_eq!(resolver.calls(), 1);
    assert!(
        sender.requests().is_empty(),
        "an empty DNS response must not create an outbound unsubscribe request"
    );
}

#[tokio::test]
async fn one_click_resolution_timeout_or_error_never_reaches_the_sender() {
    let url = Url::parse("https://unsubscribe.example.test/token").unwrap();
    let timeout_resolver = ScriptedOneClickResolver::new([ScriptedResolverOutcome::Pending]);
    let timeout_sender = RecordingOneClickSender::new(204);
    let timeout_error = post_one_click_unsubscribe_with_resolver(
        &url,
        &timeout_resolver,
        Duration::from_millis(10),
        &timeout_sender,
    )
    .await
    .unwrap_err();
    assert!(timeout_error
        .to_string()
        .contains("unsubscribe DNS lookup timed out"));
    assert_eq!(timeout_resolver.calls(), 1);
    assert!(timeout_sender.requests().is_empty());

    let error_resolver =
        ScriptedOneClickResolver::new([ScriptedResolverOutcome::Error("fictional DNS failure")]);
    let error_sender = RecordingOneClickSender::new(204);
    let resolver_error = post_one_click_unsubscribe_with_resolver(
        &url,
        &error_resolver,
        Duration::from_secs(1),
        &error_sender,
    )
    .await
    .unwrap_err();
    assert!(resolver_error
        .to_string()
        .contains("could not resolve unsubscribe host"));
    assert_eq!(error_resolver.calls(), 1);
    assert!(
        error_sender.requests().is_empty(),
        "a failed lookup must not create an outbound unsubscribe request"
    );
}

#[tokio::test]
async fn one_click_resolver_rejects_private_and_mixed_ipv6_answers() {
    let url = Url::parse("https://unsubscribe.example.test/token").unwrap();
    for private in [
        "::1",
        "::ffff:127.0.0.1",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
    ] {
        let resolver = ScriptedOneClickResolver::new([ScriptedResolverOutcome::Addresses(vec![
            SocketAddr::new(private.parse().unwrap(), 443),
        ])]);
        let sender = RecordingOneClickSender::new(204);
        assert!(
            post_one_click_unsubscribe_with_resolver(
                &url,
                &resolver,
                Duration::from_secs(1),
                &sender,
            )
            .await
            .is_err(),
            "private IPv6 answer {private} must fail closed"
        );
        assert!(sender.requests().is_empty());
    }

    let public_v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443);
    let public_v6 = SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 443);
    let private_v6 = SocketAddr::new("fc00::1".parse().unwrap(), 443);
    for answers in [vec![public_v4, private_v6], vec![public_v6, private_v6]] {
        let resolver = ScriptedOneClickResolver::new([ScriptedResolverOutcome::Addresses(answers)]);
        let sender = RecordingOneClickSender::new(204);
        assert!(
            post_one_click_unsubscribe_with_resolver(
                &url,
                &resolver,
                Duration::from_secs(1),
                &sender,
            )
            .await
            .is_err(),
            "mixed public and private answers must fail closed"
        );
        assert!(sender.requests().is_empty());
    }

    let resolver = ScriptedOneClickResolver::new([ScriptedResolverOutcome::Addresses(vec![
        public_v4, public_v6,
    ])]);
    let sender = RecordingOneClickSender::new(204);
    post_one_click_unsubscribe_with_resolver(&url, &resolver, Duration::from_secs(1), &sender)
        .await
        .unwrap();
    assert_eq!(sender.requests()[0].addresses, vec![public_v4, public_v6]);
}

#[tokio::test]
async fn production_one_click_entry_rejects_loopback_before_a_socket_is_opened() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "https://{}/unsubscribe",
        listener.local_addr().unwrap()
    ))
    .unwrap();

    let error = post_one_click_unsubscribe(&url).await.unwrap_err();
    assert!(error.to_string().contains("safe HTTPS URL"));
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "an unsafe loopback URL reached a local service"
    );
}

#[tokio::test]
async fn redirects_and_non_success_responses_are_failures_without_a_followup_request() {
    let url = Url::parse("https://unsubscribe.example.test/token").unwrap();
    let (host, _) = one_click_target(&url).unwrap();
    let addresses =
        public_unsubscribe_addresses([SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)), 443)])
            .unwrap();

    for status in [301, 302, 307, 308, 400, 500] {
        let sender = RecordingOneClickSender::new(status);
        let error = post_one_click_unsubscribe_with_sender(&url, host, &addresses, &sender)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
        assert_eq!(
            sender.requests().len(),
            1,
            "a {status} response must not be followed as an unvalidated URL"
        );
    }
}

async fn reqwest_one_click_request_to_fictional_tls_service(
    response: String,
) -> (u16, String, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (acceptor, root) =
        super::tls_certificate_safety_tests::trusted_fictional_unsubscribe_tls_fixture();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(socket).await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "client closed before completing HTTP request");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n")
                && request.ends_with(b"List-Unsubscribe=One-Click")
            {
                break;
            }
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        drop(stream);
        let no_followup_connection = timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err();
        (request, no_followup_connection)
    });
    let hostname = "unresolvable-unsubscribe.invalid";
    let url = Url::parse(&format!("https://{hostname}:{port}/one-click")).unwrap();
    let addresses = [SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)];
    let sender = ReqwestOneClickSender::with_test_root(&root);
    let status = sender
        .post(OneClickRequest {
            url: &url,
            host: hostname,
            addresses: &addresses,
            content_type: "application/x-www-form-urlencoded",
            body: "List-Unsubscribe=One-Click",
        })
        .await
        .unwrap();
    let (request, no_followup_connection) = server.await.unwrap();
    (
        status,
        String::from_utf8(request).unwrap(),
        no_followup_connection,
    )
}

fn assert_exact_rfc8058_wire_request(request: &str) {
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request must terminate its headers");
    assert!(headers.starts_with("POST /one-click HTTP/1.1\r\n"));
    let header = |name: &str| {
        headers.lines().find_map(|line| {
            let (actual_name, value) = line.split_once(':')?;
            actual_name
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    };
    assert_eq!(
        header("content-type"),
        Some("application/x-www-form-urlencoded"),
        "one-click must use the RFC 8058 form type"
    );
    assert_eq!(body, "List-Unsubscribe=One-Click");
    assert_eq!(
        header("content-length"),
        Some(body.len().to_string().as_str()),
        "the declared body length must match the exact one-click form"
    );
    for forbidden in ["authorization", "cookie", "origin", "referer"] {
        assert!(
            header(forbidden).is_none(),
            "a one-click request must not include a {forbidden} context header"
        );
    }
}

#[tokio::test]
async fn reqwest_sender_posts_exact_rfc8058_body_without_context_headers() {
    let (status, request, no_followup_connection) =
        reqwest_one_click_request_to_fictional_tls_service(
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        )
        .await;
    assert_eq!(status, 204);
    assert_exact_rfc8058_wire_request(&request);
    assert!(
        no_followup_connection,
        "a successful one-click submission must issue only one request"
    );
}

#[tokio::test]
async fn reqwest_sender_returns_redirect_without_context_or_a_followup_request() {
    let (status, request, no_followup_connection) = reqwest_one_click_request_to_fictional_tls_service(
        "HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    )
    .await;
    assert_eq!(
        status, 302,
        "a followed redirect would not return the first response"
    );
    assert_exact_rfc8058_wire_request(&request);
    assert!(
        no_followup_connection,
        "a redirect must not create a second request to an unvalidated location"
    );
}
