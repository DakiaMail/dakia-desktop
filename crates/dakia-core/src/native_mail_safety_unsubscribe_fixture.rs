//! Explicit fixture-only support for the desktop unsubscribe command TLS test.
//!
//! This module is compiled only in `native-mail-safety-fixture` builds. It
//! signs a deterministic fictional message and verifies it through a local
//! synthetic DNS authority before returning the same parsed summary that
//! production persistence consumes. It never reads environment variables or
//! creates a network client.

use crate::{mail::parse_message, Account};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use mail_auth::{
    common::crypto::{RsaKey, Sha256},
    dkim::DkimSigner,
    MessageAuthenticator,
};
use std::{net::Ipv4Addr, time::Duration};
use tokio::{net::UdpSocket, time::timeout};
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs1KeyDer};
use url::Url;

use crate::storage::MailSummary;

pub const NATIVE_MAIL_SAFETY_ONE_CLICK_HOST: &str = "unsubscribe-fixture.invalid";

// Generated solely for this non-default fictional test fixture. It has no
// relationship to an account, provider, or production signing identity.
const PRIVATE_KEY_DER_BASE64: &str = "MIIEowIBAAKCAQEA3I6SFyvPtIu3cDRNzeV1FqxH0F4Of3A2VVB/0UMCDb+z0Z8U7s7Na3gTOk4pFvtzaqTXSLgaYzh0/eLSgsbZz67xnvlGazkH12UjK3NuMbmx3IW0mviNRhqC8iHeoCmgzsLKNreaFkureQt3bhdh95YyE8fr6XsFUM1FtGBLaJlP61mPBb1Th+SCpQGtml1riefiRaRSnwsXfj+c14SBxLZqj62ZHASquvV7+bKtCDeiRfzwMXJelQu2L14QXICvPo4k2Oin6s+oT61D7I4FlYo8rC/7Z82F+BV61osNokLTobS5RuIgZUqXUk00dbazen5g5X0Xa01WYm67JC6kVQIDAQABAoIBACoOobr7ZrsF3W/u+VsmwyIUrgkEtn0El8fC9uyfe3fFs3Nog2RaWP+CQSBgISO5xTNzxX2fZaC/9kcnhn7bjlM1B+wJmqz9jgo7xe4WL9AZmdn0QRFADihtC8VCQiKGbZyftlGixyYzPbpyPk7m9I5yvI/bEphOFiBnVEBmEFscbkC/0ZIkGg1TV3xhKIld66bFbry1Bn6nJ4z3el0e6kkovUYQRuecYthD6BP6JId5iNmJ72m3LQpN+mvG31xh6XLuwG5Nt5BY/2bpM/AXmOQZRTkUFzer7WHDJTTwDHDZE6ghH0+hE33HHvYB7WVXeUawQLYTqQaefhUoxPP+Z00CgYEA/FKG/ishf/GaYVZJMf5U6zn9vkeGeRVi6JGr/LCN7MFB7r+6+t7LQJMB6CVCAN3DVoTytmiNhHJ+jFaG8SyX+O4x2RmNmoxigLClGRuqc7/+ToAM1n8PiaJIBoP/j+cqDNHQDL7WK7dwNi6PLa5wG+HJiI+yFm14eFj9ukc0ygMCgYEA38WE5qVFzm/FFZ3HesJE6S5G0w3K3HlBd77ajt8DtF/XartenYEF/PqFYc7dG/r4DFoL0xSdWCC6VpyRU69MlF+jXwCOXqGVvZKbDBUiO74G0BKrCPj71BvxPf6nZ4G/t+UURZmNqe7B2AgMiXI5nNEcbdkIQG12tEu2kV5fNMcCgYBw7DZB9eeOLQBcY5Nqblpu04s95yDFe87CKr9/3TwHIw5w3Rm/QIpSIfiOfm4lmRnc1K8S94OuLNd50xedQ8nXvDJO6OK4se87jNK2g+0q1FfppeEY9dBsVlZJpDVxn5DwH84uhhGviu8LMJcfaJv/vnLFTRn0c5WgtqMHBOsZJQKBgQCDFHsTeMD5enOig/g0KtSfG2KVg0DQAZ+d+4ifpHfRE0tm1DdKYCgX1tCHSNww5cSjx6OhlDPPSeydi719nfRn/dX/cp3zRQgoMlbsrvyMrnXjMOxLpz4tM+kO/bsmYNTjbid/1cEsSBX47E4bvXOOLthOaJHy26xrVOavJO70RQKBgFKGLx+YUASQcbR9BSyC0UDi58WSlOLTw970ykzmhOVG1xx7bAAlz3BEKPxIGDpEjw7rf18+m03XTebPmoVTyXWk38BmOUkxUWkZd9+c/dsfnV8UXwDUYLDzZ1nTXB3S022LX0hO+HxgiwuQ6z9+s1IHp70NykXcl0mZxsIEqsr2";
const PUBLIC_KEY_PKCS1_BASE64: &str = "MIIBCgKCAQEA3I6SFyvPtIu3cDRNzeV1FqxH0F4Of3A2VVB/0UMCDb+z0Z8U7s7Na3gTOk4pFvtzaqTXSLgaYzh0/eLSgsbZz67xnvlGazkH12UjK3NuMbmx3IW0mviNRhqC8iHeoCmgzsLKNreaFkureQt3bhdh95YyE8fr6XsFUM1FtGBLaJlP61mPBb1Th+SCpQGtml1riefiRaRSnwsXfj+c14SBxLZqj62ZHASquvV7+bKtCDeiRfzwMXJelQu2L14QXICvPo4k2Oin6s+oT61D7I4FlYo8rC/7Z82F+BV61osNokLTobS5RuIgZUqXUk00dbazen5g5X0Xa01WYm67JC6kVQIDAQAB";

/// Parse a DKIM-signed fictional one-click message through the production MIME
/// parser and a synthetic local DNS authority. The generated URL must use the
/// exact fixture host, so callers cannot turn this into a generic test URL
/// signer.
pub async fn signed_one_click_message(
    account: &Account,
    mailbox: &str,
    uid: u32,
    url: &Url,
) -> Result<MailSummary> {
    if url.scheme() != "https"
        || url.host_str() != Some(NATIVE_MAIL_SAFETY_ONE_CLICK_HOST)
        || url.path() != "/one-click"
        || url.username() != ""
        || url.password().is_some()
    {
        bail!("native mail-safety unsubscribe URL is not the exact fixture target");
    }
    let raw = signed_message(url)?;
    let (authenticator, server) = parse_message_authenticator().await?;
    let message = parse_message(account, mailbox, uid, &[], &raw, Some(&authenticator)).await?;
    timeout(Duration::from_secs(2), server)
        .await
        .context("fictional DKIM authority did not finish")???;
    if message.unsubscribe_kind.as_deref() != Some("one_click")
        || message.unsubscribe_url.as_deref() != Some(url.as_str())
    {
        bail!("signed native fixture message did not select its one-click action");
    }
    Ok(message)
}

fn signed_message(url: &Url) -> Result<Vec<u8>> {
    let raw = format!(
        "From: list@example.test\r\nTo: recipient@example.test\r\nList-Unsubscribe: <{url}>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nSubject: fictional command unsubscribe\r\nDate: Wed, 26 Aug 2026 12:00:00 +0000\r\n\r\nunsubscribe body\r\n"
    );
    let key_der = STANDARD
        .decode(PRIVATE_KEY_DER_BASE64)
        .context("decode fictional DKIM fixture key")?;
    let key =
        RsaKey::<Sha256>::from_key_der(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key_der)))
            .context("parse fictional DKIM fixture key")?;
    let signature = DkimSigner::from_key(key)
        .domain("example.test")
        .selector("safety")
        .headers([
            "From",
            "List-Unsubscribe",
            "List-Unsubscribe-Post",
            "Subject",
        ])
        .header_canonicalization(mail_auth::dkim::Canonicalization::Simple)
        .body_canonicalization(mail_auth::dkim::Canonicalization::Simple)
        .sign(raw.as_bytes())
        .context("sign fictional DKIM fixture message")?;
    let mut signed = Vec::with_capacity(raw.len() + 512);
    signature.write(&mut signed, true);
    signed.extend_from_slice(raw.as_bytes());
    Ok(signed)
}

async fn parse_message_authenticator(
) -> Result<(MessageAuthenticator, tokio::task::JoinHandle<Result<()>>)> {
    let server = UdpSocket::bind("127.0.0.1:0").await?;
    let port = server.local_addr()?.port();
    let public_record = format!("v=DKIM1; k=rsa; p={PUBLIC_KEY_PKCS1_BASE64}");
    let task = tokio::spawn(async move {
        let mut query = [0_u8; 2048];
        let (length, peer) = timeout(Duration::from_secs(1), server.recv_from(&mut query))
            .await
            .context("fictional DKIM authority timed out")??;
        if dns_question_name(&query[..length])? != "safety._domainkey.example.test." {
            bail!("fictional DKIM authority received an unexpected question");
        }
        let response = dns_txt_response(&query[..length], public_record.as_bytes())?;
        server.send_to(&response, peer).await?;
        Ok(())
    });

    let mut name_server =
        mail_auth::hickory_resolver::config::NameServerConfig::udp(Ipv4Addr::LOCALHOST.into());
    name_server.connections[0].port = port;
    let mut options = mail_auth::hickory_resolver::config::ResolverOpts::default();
    options.timeout = Duration::from_secs(1);
    options.attempts = 1;
    options.edns0 = false;
    options.num_concurrent_reqs = 1;
    let authenticator = MessageAuthenticator::new(
        mail_auth::hickory_resolver::config::ResolverConfig::from_parts(
            None,
            vec![],
            vec![name_server],
        ),
        options,
    )?;
    Ok((authenticator, task))
}

fn dns_question_name(query: &[u8]) -> Result<String> {
    if query.len() < 17 {
        bail!("fictional DNS query has no question");
    }
    let mut cursor = 12;
    let mut labels = Vec::new();
    while *query
        .get(cursor)
        .context("fictional DNS query label is missing")?
        != 0
    {
        let length = usize::from(
            *query
                .get(cursor)
                .context("fictional DNS label length is missing")?,
        );
        cursor += 1;
        if length == 0 || cursor + length > query.len() {
            bail!("fictional DNS query label is invalid");
        }
        labels.push(std::str::from_utf8(&query[cursor..cursor + length])?);
        cursor += length;
    }
    if cursor + 5 > query.len() {
        bail!("fictional DNS query is incomplete");
    }
    Ok(format!("{}.", labels.join(".")))
}

fn dns_txt_response(query: &[u8], value: &[u8]) -> Result<Vec<u8>> {
    let mut question_end = 12;
    while *query
        .get(question_end)
        .context("fictional DNS question is missing")?
        != 0
    {
        let length = usize::from(
            *query
                .get(question_end)
                .context("fictional DNS label length is missing")?,
        );
        question_end += length + 1;
    }
    question_end += 5;
    if question_end > query.len() {
        bail!("fictional DNS question is incomplete");
    }
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
    Ok(response)
}
