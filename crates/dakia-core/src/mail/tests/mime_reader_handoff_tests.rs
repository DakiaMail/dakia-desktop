use super::*;
use crate::CachedMessageContent;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

const MIME_RECOVERY_UID_VALIDITY: i64 = 7_707;

fn reader_handoff() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../testdata/mime-reader-handoff/ambiguous-inline-identities.json"
    ))
    .expect("checked-in Reader handoff fixture must be valid JSON")
}

fn corpus_semantics() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../testdata/mime-reader-handoff/corpus-semantics.json"
    ))
    .expect("checked-in corpus Reader semantics must be valid JSON")
}

fn truncated_representation_tolerance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../testdata/mime-reader-handoff/truncated-multipart-lf-representation-tolerance.json"
    ))
    .expect("checked-in truncated MIME tolerance fixture must be valid JSON")
}

fn governed_raw(name: &str) -> &'static [u8] {
    match name {
        "competing-nested-alternatives" => {
            include_bytes!("../../../testdata/mime/competing-nested-alternatives.eml")
        }
        "attached-message-rfc822" => {
            include_bytes!("../../../testdata/mime/attached-message-rfc822.eml")
        }
        "multipart-attachment-container" => {
            include_bytes!("../../../testdata/mime/multipart-attachment-container.eml")
        }
        "empty-alternative-leaf" => {
            include_bytes!("../../../testdata/mime/empty-alternative-leaf.eml")
        }
        "truncated-multipart-lf" => {
            include_bytes!("../../../testdata/mime/truncated-multipart-lf.eml")
        }
        "charset-matrix" => include_bytes!("../../../testdata/mime/charset-matrix.eml"),
        "format-flowed-delsp" => include_bytes!("../../../testdata/mime/format-flowed-delsp.eml"),
        "international-reader-semantics" => {
            include_bytes!("../../../testdata/mime/international-reader-semantics.eml")
        }
        "ambiguous-inline-identities" => {
            include_bytes!("../../../testdata/mime/ambiguous-inline-identities.eml")
        }
        "provider-signature-inline" => {
            include_bytes!("../../../tests/fixtures/provider-signature-inline.eml")
        }
        "linkedin-inline-content-id" => {
            include_bytes!("../../../testdata/mime/linkedin-inline-content-id.eml")
        }
        "invalid-transfer-encodings" => {
            include_bytes!("../../../testdata/mime/invalid-transfer-encodings.eml")
        }
        "filename-parameters-and-encodings" => {
            include_bytes!("../../../testdata/mime/filename-parameters-and-encodings.eml")
        }
        "disposition-type-name-matrix" => {
            include_bytes!("../../../testdata/mime/disposition-type-name-matrix.eml")
        }
        "standards-defaults-and-boundaries" => {
            include_bytes!("../../../testdata/mime/standards-defaults-and-boundaries.eml")
        }
        "standards-multipart-digest" => {
            include_bytes!("../../../testdata/mime/standards-multipart-digest.eml")
        }
        "standards-rfc2231-pathologies" => {
            include_bytes!("../../../testdata/mime/standards-rfc2231-pathologies.eml")
        }
        "standards-unknown-transfer-encoding" => {
            include_bytes!("../../../testdata/mime/standards-unknown-transfer-encoding.eml")
        }
        "standards-bodystructure-omitted-attachment" => {
            include_bytes!("../../../testdata/mime/standards-bodystructure-omitted-attachment.eml")
        }
        "standards-base64-final-quanta" => {
            include_bytes!("../../../testdata/mime/standards-base64-final-quanta.eml")
        }
        "standards-malformed-quoted-printable" => {
            include_bytes!("../../../testdata/mime/standards-malformed-quoted-printable.eml")
        }
        "standards-multipart-boundary-lengths" => {
            include_bytes!("../../../testdata/mime/standards-multipart-boundary-lengths.eml")
        }
        "greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction" => include_bytes!(
            "../../../testdata/mime/greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction.eml"
        ),
        _ => panic!("unknown governed MIME corpus input: {name}"),
    }
}

fn mime_recovery_account() -> Account {
    let mut account = imap_test_account(Security::Tls, 993);
    account.id = uuid::Uuid::parse_str("21212121-2121-4121-8121-212121212121")
        .expect("fixed fictional account UUID");
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    account
}

async fn seed_mime_recovery_catalogue(store: &Store, account: &Account, uids: &[u32]) {
    store
        .save_mailbox_catalog_state(
            account.id,
            "INBOX",
            "INBOX",
            MIME_RECOVERY_UID_VALIDITY as u32,
            uids.len(),
            true,
        )
        .await
        .expect("save fictional mailbox state");
    for uid in uids {
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: MIME recovery fixture {uid}\r\nMessage-ID: <mime-recovery-{uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
        );
        let mut message = parse_catalog_message(
            account,
            "INBOX",
            *uid,
            &[],
            headers.as_bytes(),
            "catalogue preview".into(),
        )
        .expect("parse fictional catalogue row");
        message.mailbox_uid_validity = Some(MIME_RECOVERY_UID_VALIDITY);
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .expect("save fictional catalogue row");
    }
}

async fn scripted_mime_hydration_server(
    uid: u32,
    transfer_encoding: &'static str,
    body: &'static [u8],
    advertised_size: usize,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional MIME IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: MIME recovery fixture {uid}\r\nMessage-ID: <mime-recovery-{uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: {transfer_encoding}\r\n\r\n"
        );
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional MIME IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            transcript.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 4 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with(&format!(
                "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
            )) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 12:00:00 +0000\" BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"{transfer_encoding}\" {} 1) BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            advertised_size,
                            headers.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write metadata literal");
                write
                    .write_all(format!(")\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if command == format!("UID FETCH {uid} (BODY.PEEK[TEXT])") {
                write
                    .write_all(
                        format!("* 1 FETCH (UID {uid} BODY[TEXT] {{{}}}\r\n", body.len())
                            .as_bytes(),
                    )
                    .await
                    .expect("write body prefix");
                write.write_all(body).await.expect("write body literal");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK body\r\n").as_bytes())
                    .await
                    .expect("finish body");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional MIME command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

async fn scripted_unknown_transfer_server(
    raw: &'static [u8],
    uid: u32,
    advertised_bodystructure: Option<String>,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let fixture = fixture_imap_message(raw).expect("derive unknown-transfer IMAP fixture");
    let bodystructure = advertised_bodystructure.unwrap_or_else(|| fixture.bodystructure.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional unknown-transfer IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional unknown-transfer IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.expect("read command") == 0 {
                break;
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            transcript.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 1 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with(&format!(
                "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
            )) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 10:23:00 +0000\" BODYSTRUCTURE {} BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            bodystructure,
                            fixture.root_headers.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(&fixture.root_headers)
                    .await
                    .expect("write root headers");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if let Some(section) = command
                .strip_prefix(&format!("UID FETCH {uid} (BODY.PEEK["))
                .and_then(|value| value.strip_suffix("])"))
            {
                let (path, mime_headers) = section
                    .strip_suffix(".MIME")
                    .map(|path| (path, true))
                    .unwrap_or((section, false));
                let key = if path.eq_ignore_ascii_case("TEXT") {
                    ""
                } else {
                    path
                };
                let item = fixture.sections.get(key).unwrap_or_else(|| {
                    panic!("fictional server lacks BODY[{section}] in {command}")
                });
                let bytes = if mime_headers {
                    &item.headers
                } else {
                    &item.body
                };
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} BODY[{section}] {{{}}}\r\n",
                            bytes.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write section prefix");
                write.write_all(bytes).await.expect("write section bytes");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK section\r\n").as_bytes())
                    .await
                    .expect("finish section");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional unknown-transfer command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

const GREENMAIL_UID6_BODYSTRUCTURE: &str = concat!(
    "(((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"quoted-printable\" 172 4 NIL NIL NIL)",
    "(\"text\" \"html\" (\"charset\" \"utf-8\") NIL NIL \"quoted-printable\" 171 4 NIL NIL NIL)",
    " \"alternative\" (\"boundary\" \"lsuiFkGwC3aYPCzfOOXQAmDjU18CrsM9KqFFSB0h\") NIL NIL)",
    " \"mixed\" (\"boundary\" \"h31i6HiogmhGLvvmyZeUtGNb54SJ9QdrknPspbz1\") NIL NIL)"
);
const GREENMAIL_UID6_METADATA_HEADERS: &[u8] = concat!(
    "From: \"Fictional Reader\" <fictional-reader@localhost>\r\n",
    "Subject: Dakia fictional held-sync single-send 8VRoLm\r\n",
    "To: fictional-reader@localhost\r\n",
    "Date: Wed, 21 Oct 2026 22:19:47 +0000\r\n",
    "Content-Type: multipart/mixed;\r\n",
    " boundary=\"h31i6HiogmhGLvvmyZeUtGNb54SJ9QdrknPspbz1\"\r\n",
)
.as_bytes();
const GREENMAIL_UID6_REPEATED_PARENT_HEADERS: &[u8] = concat!(
    "Content-Type: multipart/alternative;\r\n",
    " boundary=\"lsuiFkGwC3aYPCzfOOXQAmDjU18CrsM9KqFFSB0h\"",
)
.as_bytes();

enum ContradictoryRaw {
    Present(&'static [u8]),
    Missing,
    Oversized,
}

/// Replays the observed GreenMail 2.1.14 nested MIME-header contradiction:
/// BODYSTRUCTURE identifies text leaves, but both leaf `.MIME` sections repeat
/// their multipart/alternative parent. The raw fallback is deliberately served
/// only after the contradiction, on the same selected connection.
async fn scripted_greenmail_uid6_topology_contradiction_server(
    raw: ContradictoryRaw,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional GreenMail MIME listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional GreenMail IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional GreenMail command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            transcript.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 1 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with("UID FETCH 6 (FLAGS INTERNALDATE BODYSTRUCTURE") {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID 6 FLAGS () INTERNALDATE \"21-Oct-2026 22:19:47 +0000\" BODYSTRUCTURE {GREENMAIL_UID6_BODYSTRUCTURE} BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            GREENMAIL_UID6_METADATA_HEADERS.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(GREENMAIL_UID6_METADATA_HEADERS)
                    .await
                    .expect("write metadata literal");
                write
                    .write_all(format!(")\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if matches!(
                command,
                "UID FETCH 6 (BODY.PEEK[1.MIME])"
                    | "UID FETCH 6 (BODY.PEEK[1.1.MIME])"
                    | "UID FETCH 6 (BODY.PEEK[1.2.MIME])"
            ) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID 6 BODY[{}] {{{}}}\r\n",
                            command
                                .trim_start_matches("UID FETCH 6 (BODY.PEEK[")
                                .trim_end_matches("])"),
                            GREENMAIL_UID6_REPEATED_PARENT_HEADERS.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write contradictory MIME prefix");
                write
                    .write_all(GREENMAIL_UID6_REPEATED_PARENT_HEADERS)
                    .await
                    .expect("write contradictory MIME headers");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK section\r\n").as_bytes())
                    .await
                    .expect("finish contradictory MIME response");
            } else if command == "UID FETCH 6 (FLAGS INTERNALDATE BODY.PEEK[])" {
                match raw {
                    ContradictoryRaw::Present(raw) => {
                        write
                            .write_all(
                                format!(
                                    "* 1 FETCH (UID 6 FLAGS () INTERNALDATE \"21-Oct-2026 22:19:47 +0000\" BODY[]<0> {{{}}}\r\n",
                                    raw.len()
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("write fallback raw prefix");
                        write.write_all(raw).await.expect("write fallback raw");
                        write
                            .write_all(format!("\r\n)\r\n{tag} OK raw\r\n").as_bytes())
                            .await
                            .expect("finish fallback raw");
                    }
                    ContradictoryRaw::Missing => {
                        write
                            .write_all(
                                format!("* 1 FETCH (UID 6 FLAGS ())\r\n{tag} OK missing raw\r\n")
                                    .as_bytes(),
                            )
                            .await
                            .expect("omit fallback raw literal");
                    }
                    ContradictoryRaw::Oversized => {
                        write
                            .write_all(
                                format!(
                                    "* 1 FETCH (UID 6 BODY[]<0> {{{}}}\r\n",
                                    MAX_IMAP_RESPONSE_LITERAL_BYTES + 1
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("advertise oversized fallback raw");
                    }
                }
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional GreenMail command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

#[derive(Debug)]
struct LargeResourceTranscript {
    commands: Vec<String>,
    streamed_bytes: usize,
}

#[derive(Clone, Copy)]
enum LargeAttachmentBody {
    Raw { byte: u8, bytes: usize },
    Base64Zeros { decoded: usize, encoded: usize },
    AdvertiseOnly { bytes: usize },
}

impl LargeAttachmentBody {
    fn advertised_bytes(self) -> usize {
        match self {
            Self::Raw { bytes, .. } | Self::AdvertiseOnly { bytes } => bytes,
            Self::Base64Zeros { encoded, .. } => encoded,
        }
    }

    fn transfer_encoding(self) -> &'static str {
        match self {
            Self::Base64Zeros { .. } | Self::AdvertiseOnly { .. } => "BASE64",
            Self::Raw { .. } => "X-FICTIONAL-TRANSFER",
        }
    }
}

async fn write_repeated<W>(write: &mut W, byte: u8, mut remaining: usize) -> usize
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let chunk = vec![byte; 64 * 1024];
    let total = remaining;
    while remaining > 0 {
        let next = remaining.min(chunk.len());
        write
            .write_all(&chunk[..next])
            .await
            .expect("stream fictional resource bytes");
        remaining -= next;
    }
    total
}

async fn write_base64_zeros<W>(write: &mut W, decoded: usize, encoded: usize) -> usize
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut decoded_remaining = decoded;
    let raw_chunk = vec![0_u8; 48 * 1024];
    let mut encoded_written = 0usize;
    while decoded_remaining > 0 {
        let next = decoded_remaining.min(raw_chunk.len());
        let encoded_chunk = base64::engine::general_purpose::STANDARD.encode(&raw_chunk[..next]);
        write
            .write_all(encoded_chunk.as_bytes())
            .await
            .expect("stream fictional base64 bytes");
        encoded_written += encoded_chunk.len();
        decoded_remaining -= next;
    }
    assert!(
        encoded_written <= encoded,
        "encoded boundary cannot hold the requested decoded bytes"
    );
    let mut whitespace = encoded - encoded_written;
    if whitespace % 2 == 1 {
        write
            .write_all(b"\n")
            .await
            .expect("write odd base64 folding byte");
        whitespace -= 1;
    }
    let folding = vec![b'\n'; 64 * 1024];
    while whitespace > 0 {
        let next = whitespace.min(folding.len());
        write
            .write_all(&folding[..next])
            .await
            .expect("stream base64 folding bytes");
        whitespace -= next;
    }
    encoded
}

async fn scripted_large_attachment_server(
    uid: u32,
    body: LargeAttachmentBody,
) -> (ImapEndpoint, JoinHandle<LargeResourceTranscript>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional large-attachment IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let transfer_encoding = body.transfer_encoding();
        let advertised = body.advertised_bytes();
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Large attachment {uid}\r\nMessage-ID: <large-attachment-{uid}@example.test>\r\nContent-Type: application/octet-stream; name=boundary.bin\r\nContent-Disposition: attachment; filename=boundary.bin\r\nContent-Transfer-Encoding: {transfer_encoding}\r\n\r\n"
        );
        let bodystructure = format!(
            "(\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"boundary.bin\") NIL NIL \"{transfer_encoding}\" {advertised} NIL (\"ATTACHMENT\" (\"FILENAME\" \"boundary.bin\")))"
        );
        let mut commands = Vec::new();
        let mut streamed_bytes = 0usize;
        write
            .write_all(b"* OK fictional large-attachment IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional large-attachment command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            commands.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 1 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with(&format!(
                "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
            )) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 12:00:00 +0000\" BODYSTRUCTURE {bodystructure} BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            headers.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write root headers");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if command == format!("UID FETCH {uid} (BODY.PEEK[TEXT])") {
                write
                    .write_all(
                        format!("* 1 FETCH (UID {uid} BODY[TEXT] {{{advertised}}}\r\n").as_bytes(),
                    )
                    .await
                    .expect("write attachment prefix");
                match body {
                    LargeAttachmentBody::Raw { byte, bytes } => {
                        streamed_bytes += write_repeated(&mut write, byte, bytes).await;
                    }
                    LargeAttachmentBody::Base64Zeros { decoded, encoded } => {
                        streamed_bytes += write_base64_zeros(&mut write, decoded, encoded).await;
                    }
                    LargeAttachmentBody::AdvertiseOnly { .. } => break,
                }
                write
                    .write_all(format!("\r\n)\r\n{tag} OK attachment\r\n").as_bytes())
                    .await
                    .expect("finish attachment response");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                break;
            } else {
                panic!("unexpected fictional large-attachment command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        LargeResourceTranscript {
            commands,
            streamed_bytes,
        }
    });
    (imap_test_endpoint(port, true), server)
}

async fn scripted_large_display_server(
    uid: u32,
    part_sizes: Vec<usize>,
) -> (ImapEndpoint, JoinHandle<LargeResourceTranscript>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional large-display IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Large display {uid}\r\nMessage-ID: <large-display-{uid}@example.test>\r\nContent-Type: multipart/mixed; boundary=resource-budget\r\n\r\n"
        );
        let children = part_sizes
            .iter()
            .map(|size| {
                format!(
                    "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" {size} 1 NIL NIL NIL)"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let bodystructure = format!("({children} \"MIXED\" (\"BOUNDARY\" \"resource-budget\"))");
        let mime_headers =
            b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n";
        let mut commands = Vec::new();
        let mut streamed_bytes = 0usize;
        write
            .write_all(b"* OK fictional large-display IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional large-display command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            commands.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 3 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with(&format!(
                "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
            )) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 12:00:00 +0000\" BODYSTRUCTURE {bodystructure} BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            headers.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write root headers");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if let Some(section) = command
                .strip_prefix(&format!("UID FETCH {uid} (BODY.PEEK["))
                .and_then(|value| value.strip_suffix("])"))
            {
                let mime = section.ends_with(".MIME");
                let path = section.trim_end_matches(".MIME");
                let index = path
                    .parse::<usize>()
                    .expect("numeric fictional display section")
                    .checked_sub(1)
                    .expect("one-based fictional display section");
                if mime {
                    write
                        .write_all(
                            format!(
                                "* 1 FETCH (UID {uid} BODY[{section}] {{{}}}\r\n",
                                mime_headers.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("write display MIME prefix");
                    write
                        .write_all(mime_headers)
                        .await
                        .expect("write display MIME headers");
                } else {
                    let size = part_sizes[index];
                    write
                        .write_all(
                            format!("* 1 FETCH (UID {uid} BODY[{section}] {{{size}}}\r\n")
                                .as_bytes(),
                        )
                        .await
                        .expect("write display body prefix");
                    streamed_bytes +=
                        write_repeated(&mut write, b'a' + u8::try_from(index).unwrap(), size).await;
                }
                write
                    .write_all(format!("\r\n)\r\n{tag} OK section\r\n").as_bytes())
                    .await
                    .expect("finish display section");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                break;
            } else {
                panic!("unexpected fictional large-display command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        LargeResourceTranscript {
            commands,
            streamed_bytes,
        }
    });
    (imap_test_endpoint(port, true), server)
}

async fn reopen_reader_payload(raw: &[u8], uid: u32) -> (MailSummary, Value) {
    let mut account = test_account();
    account.id = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111")
        .expect("fixture account UUID");
    let parsed = parse_message(&account, "INBOX", uid, &[], raw, None)
        .await
        .expect("governed MIME corpus input must parse");
    let directory = tempfile::tempdir().expect("temporary profile");
    let database = directory.path().join("mime-reader-semantics.sqlite");
    let store = Store::open(&database).await.expect("open profile");
    store.save_account(&account).await.expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&parsed))
        .await
        .expect("persist parsed message");
    store
        .cache_message_content(
            &parsed.id,
            parsed.is_flagged,
            CachedMessageContent {
                body_text: parsed.body_text.clone(),
                body_html: parsed.body_html.clone(),
                unsubscribe_kind: parsed.unsubscribe_kind.clone(),
                attachments: parsed
                    .attachments
                    .iter()
                    .map(|attachment| attachment.attachment.clone())
                    .collect(),
            },
        )
        .await
        .expect("persist Reader content cache");
    drop(store);

    let reopened = Store::open(&database).await.expect("reopen profile");
    let cached = reopened
        .cached_message_content(&parsed.id)
        .await
        .expect("read Reader content cache")
        .expect("complete parsed body remains available to Reader");
    let payload = json!({
        "body_text": cached.body_text,
        "body_html": cached.body_html,
        "attachments": cached.attachments.into_iter().filter(|attachment| attachment.presentation.is_downloadable()).map(|attachment| json!({
            "filename": attachment.filename,
            "presentation": attachment.presentation,
        })).collect::<Vec<_>>(),
    });
    (parsed, payload)
}

async fn complete_selective_reopen(
    raw: &[u8],
    uid: u32,
) -> (MailSummary, MailSummary, MailSummary, CachedMessageContent) {
    let account = test_account();
    let complete = parse_message(&account, "INBOX", uid, &[], raw, None)
        .await
        .expect("complete governed MIME fixture");
    let fixture = fixture_imap_message(raw).expect("derive exact selective fixture sections");
    let structure = parse_bodystructure(&[format!(
        "* 1 FETCH (UID {uid} BODYSTRUCTURE {})",
        fixture.bodystructure
    )])
    .expect("parse fixture BODYSTRUCTURE");
    let response_lines = vec![format!(
        "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 10:30:00 +0000\")"
    )];
    let (client_transport, server_transport) = duplex(MAX_RAW_MESSAGE_BYTES.min(1024 * 1024));
    let server = tokio::spawn(serve_fixture_selective_sections(
        server_transport,
        uid,
        fixture.sections,
    ));
    let mut client = ImapClient {
        reader: BufReader::new(client_transport),
        tag: 0,
    };
    let selective = fetch_selective_message(
        &mut client,
        &account,
        "INBOX",
        uid,
        &response_lines,
        &fixture.root_headers,
        &structure,
    )
    .await
    .expect("selectively fetch governed MIME fixture");
    drop(client);
    server
        .await
        .expect("selective fixture server task")
        .expect("serve selective fixture sections");

    let directory = tempfile::tempdir().expect("temporary selective MIME profile");
    let database = directory.path().join("standards-selective-reopen.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open selective profile");
    store.save_account(&account).await.expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&selective))
        .await
        .expect("persist selectively fetched summary");
    store
        .cache_message_content(
            &selective.id,
            selective.is_flagged,
            CachedMessageContent {
                body_text: selective.body_text.clone(),
                body_html: selective.body_html.clone(),
                unsubscribe_kind: selective.unsubscribe_kind.clone(),
                attachments: selective
                    .attachments
                    .iter()
                    .map(|attachment| attachment.attachment.clone())
                    .collect(),
            },
        )
        .await
        .expect("cache selectively fetched Reader content");
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen selective profile");
    let reopened_summary = reopened
        .message(&selective.id)
        .await
        .expect("read reopened selective summary")
        .expect("selective summary survives reopen");
    let reopened_content = reopened
        .cached_message_content(&selective.id)
        .await
        .expect("read reopened selective Reader content")
        .expect("selective Reader content survives reopen");
    (complete, selective, reopened_summary, reopened_content)
}

#[tokio::test]
async fn governed_mime_corpus_survives_parse_reopen_and_declared_reader_semantics() {
    let semantics = corpus_semantics();
    for (index, name) in [
        "competing-nested-alternatives",
        "attached-message-rfc822",
        "multipart-attachment-container",
        "empty-alternative-leaf",
        "truncated-multipart-lf",
        "charset-matrix",
        "format-flowed-delsp",
        "international-reader-semantics",
    ]
    .into_iter()
    .enumerate()
    {
        let raw = governed_raw(name);
        let uid = 100 + index as u32;
        let (parsed, payload) = reopen_reader_payload(raw, uid).await;
        let expected = &semantics["scenarios"][name];

        // Header and catalogue paths intentionally avoid MIME-body decoding,
        // but must retain the same declared message identity and subject.
        let header =
            parse_header_message(&test_account(), "INBOX", uid, &[], raw).expect("header path");
        assert_eq!(header.subject, parsed.subject, "{name} header subject");
        assert_eq!(
            parse_catalog_message(
                &test_account(),
                "INBOX",
                uid,
                &[],
                raw,
                "catalogue preview".into(),
            )
            .expect("catalogue path")
            .subject,
            parsed.subject,
            "{name} catalogue subject"
        );
        if let Some(headers) = expected.get("headers") {
            assert_eq!(
                header.subject,
                headers["subject"].as_str().unwrap(),
                "{name} decoded RFC 2047 subject"
            );
            assert_eq!(
                header.from_name.as_deref(),
                headers["fromName"].as_str(),
                "{name} decoded RFC 2047 display name"
            );
            assert!(
                header
                    .to_addresses
                    .contains(headers["toAddressesInclude"].as_str().unwrap()),
                "{name} folded recipient header"
            );
        }
        for expected_text in expected["bodyTextIncludes"].as_array().unwrap() {
            assert!(
                payload["body_text"]
                    .as_str()
                    .unwrap()
                    .contains(expected_text.as_str().unwrap()),
                "{name} missing expected body text {expected_text}"
            );
        }
        for forbidden_text in expected["bodyTextExcludes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            assert!(
                !payload["body_text"]
                    .as_str()
                    .unwrap()
                    .contains(forbidden_text.as_str().unwrap()),
                "{name} leaked excluded body text {forbidden_text}"
            );
        }
        for expected_html in expected["bodyHtmlIncludes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            assert!(
                payload["body_html"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(expected_html.as_str().unwrap()),
                "{name} missing expected HTML {expected_html}"
            );
        }
        assert_eq!(
            payload["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|attachment| attachment["filename"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected["downloadableFilenames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|filename| filename.as_str().unwrap())
                .collect::<Vec<_>>(),
            "{name} downloadable attachment presentation"
        );
    }
}

#[tokio::test]
async fn invalid_transfer_encoding_returns_a_bounded_complete_error_while_metadata_stays_readable()
{
    let raw = governed_raw("invalid-transfer-encodings");
    let account = test_account();
    let semantics = corpus_semantics();
    assert_eq!(
        parse_message(&account, "INBOX", 200, &[], raw, None)
            .await
            .expect_err("invalid transfer encoding must reject full content")
            .to_string(),
        semantics["invalidTransfer"]["completeError"],
    );
    assert_eq!(
        parse_header_message(&account, "INBOX", 200, &[], raw)
            .expect("header-only path remains readable")
            .subject,
        "Synthetic invalid transfer encodings"
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 200, &[], raw, "preview".into())
            .expect("catalogue path remains readable")
            .snippet,
        "preview"
    );
}

#[tokio::test]
async fn filename_and_disposition_fixtures_preserve_exact_attachment_identity_and_size_on_reopen() {
    let account = mime_recovery_account();
    let directory = tempfile::tempdir().expect("temporary fictional profile");
    let database = directory.path().join("mime-attachment-matrix.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    store.save_account(&account).await.expect("save account");

    let cases = [
        (
            "filename-parameters-and-encodings",
            vec![
                "quarterly report final.pdf",
                "report-test.txt",
                "Prügimaja plaan vaated 31.08.pdf",
                "Prügimaja_31.08.pdf",
                "Prügimaja.pdf",
                "invoice;final.pdf",
                "invoicegpj.exe",
                "broken%ZZname.pdf",
            ],
        ),
        (
            "disposition-type-name-matrix",
            vec!["named-inline.txt", "attachment", "attachment", "attachment"],
        ),
    ];

    let mut expected_by_message = Vec::new();
    for (index, (name, expected_filenames)) in cases.into_iter().enumerate() {
        let mut message = parse_message(
            &account,
            "INBOX",
            300 + index as u32,
            &[],
            governed_raw(name),
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{name} complete parse failed: {error}"));
        message.mailbox_uid_validity = Some(MIME_RECOVERY_UID_VALIDITY);
        let filenames = message
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.filename.as_str())
            .collect::<Vec<_>>();
        assert_eq!(filenames, expected_filenames, "{name} attachment order");
        assert_eq!(
            message
                .attachments
                .iter()
                .map(|attachment| attachment.attachment.id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            message.attachments.len(),
            "{name} MIME section identities must remain distinct"
        );
        for attachment in &message.attachments {
            assert_eq!(
                attachment.attachment.size_bytes,
                attachment.bytes.len() as i64,
                "{name} {} decoded size",
                attachment.attachment.filename
            );
            assert_eq!(
                attachment.attachment.size_kind,
                AttachmentSizeKind::Decoded,
                "{name} {} complete-parser size kind",
                attachment.attachment.filename
            );
        }
        let expected = message
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.clone())
            .collect::<Vec<_>>();
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .expect("persist parsed fixture");
        store
            .cache_message_content(
                &message.id,
                message.is_flagged,
                CachedMessageContent {
                    body_text: message.body_text.clone(),
                    body_html: message.body_html.clone(),
                    unsubscribe_kind: message.unsubscribe_kind.clone(),
                    attachments: expected.clone(),
                },
            )
            .await
            .expect("persist Reader fixture cache");
        expected_by_message.push((name, message.id, expected));
    }
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    for (name, message_id, expected) in expected_by_message {
        let cached = reopened
            .cached_message_content(&message_id)
            .await
            .expect("read cached fixture")
            .expect("fixture cache survives reopen");
        assert_eq!(
            serde_json::to_value(cached.attachments).expect("serialize cached attachments"),
            serde_json::to_value(expected).expect("serialize expected attachments"),
            "{name} attachment metadata"
        );
    }
}

#[tokio::test]
async fn rfc2045_defaults_case_and_transfer_boundaries_survive_all_reader_publication_paths() {
    let raw = governed_raw("standards-defaults-and-boundaries");
    let parsed = parse_complete_message(raw).expect("parse governed RFC 2045 vector");
    let root = parsed.part(0).expect("multipart root");
    assert_eq!(mime_type(root), "multipart/x-dakia-unknown");
    let children = root.sub_parts().expect("multipart children");
    assert_eq!(children.len(), 3);

    let defaulted = parsed.part(children[0]).expect("defaulted text leaf");
    assert!(defaulted.content_type().is_none());
    assert!(defaulted.content_transfer_encoding().is_none());
    assert_eq!(mime_type(defaulted), "text/plain");
    assert_eq!(
        decode_text_candidate(&parsed, defaulted).as_deref(),
        Ok("Default ASCII body with absent content headers.")
    );

    let mixed_case = parsed.part(children[1]).expect("mixed-case text leaf");
    assert_eq!(mime_type(mixed_case), "text/plain");
    assert_eq!(
        mixed_case
            .content_type()
            .and_then(|value| mime_attribute(value, "charset")),
        Some("Us-AsCiI")
    );
    assert_eq!(
        mixed_case.content_transfer_encoding(),
        Some("QuOtEd-PrInTaBlE")
    );
    let mixed_case_text =
        decode_text_candidate(&parsed, mixed_case).expect("valid mixed-case quoted printable");
    let expected_qp = "Quoted printable softbreak keeps encoded spaces.\nHard line survives.";
    assert_eq!(mixed_case_text, expected_qp);

    let base64 = parsed.part(children[2]).expect("base64 text leaf");
    assert_eq!(base64.content_transfer_encoding(), Some("BaSe64"));
    assert_eq!(
        decode_text_candidate(&parsed, base64).as_deref(),
        Ok("Base64 boundary.")
    );

    let account = test_account();
    let complete = parse_message(&account, "INBOX", 610, &[], raw, None)
        .await
        .expect("publish governed RFC 2045 vector");
    assert_eq!(complete.subject, "Adjacent words");
    assert!(complete
        .body_text
        .contains("Default ASCII body with absent content headers."));
    assert!(complete.body_text.contains(expected_qp));
    assert!(complete.body_text.contains("Base64 boundary."));
    for metadata in [
        parse_header_message(&account, "INBOX", 610, &[], raw).expect("header path"),
        parse_catalog_message(&account, "INBOX", 610, &[], raw, "preview".into())
            .expect("catalogue path"),
    ] {
        assert_eq!(metadata.subject, "Adjacent words");
    }

    let (summary, reopened) = reopen_reader_payload(raw, 610).await;
    assert_eq!(summary.subject, "Adjacent words");
    assert_eq!(
        reopened["body_text"].as_str(),
        Some(complete.body_text.as_str())
    );
    assert!(reopened["body_text"]
        .as_str()
        .is_some_and(|body| body.contains(expected_qp)));

    let (selective_complete, selective, reopened_summary, reopened_content) =
        complete_selective_reopen(raw, 619).await;
    assert_eq!(
        selective_user_visible_semantics(&selective),
        selective_user_visible_semantics(&selective_complete),
        "valid quoted-printable soft breaks and trailing whitespace must agree across complete and selective publication"
    );
    assert!(selective_complete.body_text.contains(expected_qp));
    assert!(selective.body_text.contains(expected_qp));
    assert_eq!(reopened_summary.subject, selective_complete.subject);
    assert_eq!(reopened_content.body_text, selective.body_text);
    assert!(reopened_content.body_text.contains(expected_qp));
}

#[tokio::test]
async fn rfc2045_base64_final_quanta_and_transport_whitespace_survive_selective_reopen() {
    let raw = governed_raw("standards-base64-final-quanta");
    let parsed = parse_complete_message(raw).expect("parse governed Base64 quanta fixture");
    let children = parsed
        .part(0)
        .and_then(MessagePart::sub_parts)
        .expect("Base64 fixture children");
    assert_eq!(children.len(), 4);
    let decoded = children
        .iter()
        .map(|part_id| {
            decode_text_candidate(&parsed, parsed.part(*part_id).expect("Base64 leaf"))
                .expect("canonical Base64 leaf")
        })
        .collect::<Vec<_>>();
    assert_eq!(decoded, ["A", "AB", "ABC", "White space"]);

    let (complete, selective, reopened_summary, reopened_content) =
        complete_selective_reopen(raw, 616).await;
    let account = test_account();
    assert_eq!(
        parse_header_message(&account, "INBOX", 616, &[], raw)
            .expect("Base64 header path")
            .subject,
        complete.subject
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 616, &[], raw, "Base64 preview".into())
            .expect("Base64 catalogue path")
            .snippet,
        "Base64 preview"
    );
    assert_eq!(
        selective_user_visible_semantics(&selective),
        selective_user_visible_semantics(&complete),
        "complete and selective decoding must agree for every canonical final quantum"
    );
    let positions = ["A", "AB", "ABC", "White space"].map(|text| {
        complete
            .body_text
            .find(text)
            .unwrap_or_else(|| panic!("complete Reader body omitted {text}"))
    });
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(complete.attachments.is_empty());
    assert!(selective.attachments.is_empty());
    assert!(!complete.snippet.contains("QQ=="));
    assert!(!complete.snippet.contains("QUI="));
    assert_eq!(reopened_summary.subject, complete.subject);
    assert_eq!(reopened_content.body_text, selective.body_text);
    assert_eq!(reopened_content.body_html, selective.body_html);
    assert!(reopened_content.attachments.is_empty());
}

#[tokio::test]
async fn malformed_quoted_printable_is_bounded_rejection_across_complete_selective_and_reopen() {
    let raw = governed_raw("standards-malformed-quoted-printable");
    let parsed = parse_complete_message(raw).expect("parse malformed quoted-printable fixture");
    let root = parsed.part(0).expect("quoted-printable root");
    assert_eq!(root.content_transfer_encoding(), Some("quoted-printable"));
    assert_eq!(
        decode_text_candidate(&parsed, root),
        Err(()),
        "the body decoder must not invent content for the illegal =QZ escape"
    );

    // RFC 2045 calls =QZ formally illegal and permits, but does not require,
    // literal preservation by robust decoders. Dakia's explicit policy is a
    // bounded content error while the metadata and existing cache stay safe.
    let account = mime_recovery_account();
    assert_eq!(
        parse_message(&account, "INBOX", 617, &[], raw, None)
            .await
            .expect_err("complete Reader publication rejects malformed quoted-printable")
            .to_string(),
        MIME_CONTENT_UNDECODABLE
    );
    assert_eq!(
        parse_header_message(&account, "INBOX", 617, &[], raw)
            .expect("malformed body does not hide header metadata")
            .subject,
        "Synthetic malformed quoted printable"
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 617, &[], raw, "safe preview".into())
            .expect("malformed body does not hide catalogue metadata")
            .snippet,
        "safe preview"
    );

    let valid = parse_message(
        &account,
        "INBOX",
        616,
        &[],
        governed_raw("standards-base64-final-quanta"),
        None,
    )
    .await
    .expect("parse neighboring valid fixture");
    let directory = tempfile::tempdir().expect("temporary QP recovery profile");
    let database = directory.path().join("malformed-qp-recovery.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open QP recovery profile");
    store.save_account(&account).await.expect("save account");
    seed_mime_recovery_catalogue(&store, &account, &[617]).await;
    store
        .upsert_messages(std::slice::from_ref(&valid))
        .await
        .expect("persist valid neighboring message");
    store
        .cache_message_content(
            &valid.id,
            valid.is_flagged,
            CachedMessageContent {
                body_text: valid.body_text.clone(),
                body_html: valid.body_html.clone(),
                unsubscribe_kind: valid.unsubscribe_kind.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("cache valid neighboring Reader body");
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");
    let malformed_body = b"Malformed escape =QZ must not become invented text.";
    let (endpoint, server) = scripted_mime_hydration_server(
        617,
        "quoted-printable",
        malformed_body,
        malformed_body.len(),
    )
    .await;
    assert_eq!(
        service
            .hydrate_message_with_imap_endpoint_for_test(
                &account,
                "INBOX",
                617,
                MIME_RECOVERY_UID_VALIDITY,
                endpoint,
            )
            .await
            .expect_err("production hydration rejects malformed quoted-printable")
            .to_string(),
        MIME_CONTENT_UNDECODABLE
    );
    let transcript = server.await.expect("malformed QP fictional server");
    assert!(transcript
        .iter()
        .any(|command| command.starts_with("UID FETCH 617 (FLAGS INTERNALDATE BODYSTRUCTURE")));
    assert!(transcript.contains(&"UID FETCH 617 (BODY.PEEK[TEXT])".into()));
    drop(service);
    let reopened = Store::open(&database)
        .await
        .expect("reopen QP recovery profile");
    assert_eq!(
        reopened
            .cached_message_content(&valid.id)
            .await
            .expect("read neighboring valid cache")
            .expect("neighboring valid cache survives malformed body")
            .body_text,
        valid.body_text
    );
    let malformed_id = stable_message_id(account.id, "INBOX", 617);
    assert!(
        reopened
            .message(&malformed_id)
            .await
            .expect("read malformed message state")
            .is_some(),
        "catalogue metadata must survive failed hydration"
    );
    assert!(
        reopened
            .cached_message_content(&malformed_id)
            .await
            .expect("read malformed Reader cache state")
            .is_none(),
        "failed hydration must not publish partial Reader content"
    );
}

#[tokio::test]
async fn rfc2046_quoted_boundary_punctuation_and_69_70_lengths_survive_selective_reopen() {
    let raw = governed_raw("standards-multipart-boundary-lengths");
    let parsed = parse_complete_message(raw).expect("parse governed multipart boundaries");
    let root = parsed.part(0).expect("multipart root");
    assert_eq!(
        root.content_type()
            .and_then(|value| mime_attribute(value, "boundary")),
        Some("quoted ()+_,-./:=? boundary")
    );
    let children = root.sub_parts().expect("nested multipart children");
    assert_eq!(children.len(), 3);
    let lengths = children
        .iter()
        .map(|part_id| {
            parsed
                .part(*part_id)
                .and_then(MessagePart::content_type)
                .and_then(|value| mime_attribute(value, "boundary"))
                .expect("nested boundary")
                .len()
        })
        .collect::<Vec<_>>();
    assert_eq!(lengths, [69, 70, 71]);

    let (complete, selective, reopened_summary, reopened_content) =
        complete_selective_reopen(raw, 618).await;
    let account = test_account();
    assert_eq!(
        parse_header_message(&account, "INBOX", 618, &[], raw)
            .expect("boundary header path")
            .subject,
        complete.subject
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 618, &[], raw, "boundary preview".into())
            .expect("boundary catalogue path")
            .snippet,
        "boundary preview"
    );
    assert_eq!(
        selective_user_visible_semantics(&selective),
        selective_user_visible_semantics(&complete)
    );
    let ordered_text = [
        "Boundary length 69.",
        "Boundary length 70.",
        "Boundary length 71 is accepted as a documented parser tolerance.",
    ];
    let positions = ordered_text.map(|text| {
        complete
            .body_text
            .find(text)
            .unwrap_or_else(|| panic!("missing {text}"))
    });
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(complete.attachments.is_empty());
    assert!(selective.attachments.is_empty());
    // RFC 2046 limits boundary values to 70 characters. Accepting the final
    // 71-character child is an explicit interoperability tolerance, not a
    // claim that the fixture itself conforms to the standard.
    assert_eq!(reopened_summary.subject, complete.subject);
    assert_eq!(reopened_content.body_text, selective.body_text);
    assert!(reopened_content.attachments.is_empty());
}

#[tokio::test]
async fn rfc2046_digest_children_default_to_message_rfc822() {
    let digest_raw = governed_raw("standards-multipart-digest");
    let digest = parse_complete_message(digest_raw).expect("parse governed multipart/digest");
    let children = digest
        .part(0)
        .and_then(MessagePart::sub_parts)
        .expect("digest children");
    assert_eq!(children.len(), 2);
    for child in children {
        let part = digest.part(*child).expect("digest child");
        assert!(
            part.is_message(),
            "digest child must be an encapsulated message"
        );
        assert_eq!(mime_type(part), "message/rfc822");
        assert!(part
            .message()
            .and_then(|message| message.subject())
            .is_some_and(|subject| subject.contains("encapsulated message")));
    }
    let account = test_account();
    let published = parse_message(&account, "INBOX", 611, &[], digest_raw, None)
        .await
        .expect("publish governed multipart/digest");
    assert_eq!(published.attachments.len(), 2);
    assert_eq!(
        published
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.attachment.filename.as_str(),
                    attachment.attachment.mime_type.as_str(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("attached-message.eml", "message/rfc822"),
            ("attached-message.eml", "message/rfc822"),
        ]
    );
    assert_ne!(
        published.attachments[0].attachment.id,
        published.attachments[1].attachment.id
    );
    assert_eq!(
        published.attachments[0].bytes,
        b"Date: Tue, 28 Jul 2026 09:00:00 +0000\nFrom: first@example.test\nTo: archive@example.test\nSubject: First encapsulated message\n\nFirst encapsulated body."
    );
    assert_eq!(
        published.attachments[1].bytes,
        b"Date: Tue, 28 Jul 2026 09:01:00 +0000\nFrom: second@example.test\nTo: archive@example.test\nSubject: Explicit encapsulated message\n\nSecond encapsulated body."
    );
    assert_eq!(
        parse_header_message(&account, "INBOX", 611, &[], digest_raw)
            .expect("digest header path")
            .subject,
        "Synthetic multipart digest defaults"
    );
    assert_eq!(
        parse_catalog_message(
            &account,
            "INBOX",
            611,
            &[],
            digest_raw,
            "digest preview".into(),
        )
        .expect("digest catalogue path")
        .snippet,
        "digest preview"
    );

    let directory = tempfile::tempdir().expect("temporary digest profile");
    let database = directory.path().join("multipart-digest.sqlite");
    let store = Store::open(&database).await.expect("open digest profile");
    store.save_account(&account).await.expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&published))
        .await
        .expect("persist digest summary");
    let expected = published
        .attachments
        .iter()
        .map(|attachment| attachment.attachment.clone())
        .collect::<Vec<_>>();
    store
        .cache_message_content(
            &published.id,
            published.is_flagged,
            CachedMessageContent {
                body_text: published.body_text.clone(),
                body_html: published.body_html.clone(),
                unsubscribe_kind: published.unsubscribe_kind.clone(),
                attachments: expected.clone(),
            },
        )
        .await
        .expect("cache digest Reader metadata");
    drop(store);
    let reopened = Store::open(&database).await.expect("reopen digest profile");
    let cached = reopened
        .cached_message_content(&published.id)
        .await
        .expect("read reopened digest cache")
        .expect("digest cache survives reopen");
    assert_eq!(
        serde_json::to_value(cached.attachments).expect("serialize reopened digest attachments"),
        serde_json::to_value(expected).expect("serialize expected digest attachments")
    );
}

#[tokio::test]
async fn rfc2046_unknown_multipart_subtype_preserves_mixed_child_order() {
    let unknown = parse_message(
        &test_account(),
        "INBOX",
        611,
        &[],
        governed_raw("standards-defaults-and-boundaries"),
        None,
    )
    .await
    .expect("unknown multipart subtype follows mixed semantics");
    let plain = unknown.body_text.as_str();
    let defaulted = plain
        .find("Default ASCII body")
        .expect("first ordered child");
    let quoted = plain
        .find("Quoted printable softbreak")
        .expect("second ordered child");
    let base64 = plain.find("Base64 boundary.").expect("third ordered child");
    assert!(defaulted < quoted && quoted < base64);
}

#[tokio::test]
async fn rfc2231_language_and_malformed_continuations_have_exact_complete_selective_parity() {
    let raw = governed_raw("standards-rfc2231-pathologies");
    let account = test_account();
    let complete = parse_message(&account, "INBOX", 612, &[], raw, None)
        .await
        .expect("complete RFC 2231 vector");
    assert_eq!(
        complete
            .attachments
            .iter()
            .map(|attachment| attachment.attachment.filename.as_str())
            .collect::<Vec<_>>(),
        vec![
            "café.pdf",
            "unsupported-fallback.bin",
            "ordered-middle-tail.txt",
            "missing-fallback.bin",
            "duplicate-fallback.bin",
            "long-name.txt",
            "quote.txt",
            "slash.txt",
            "mixed-quote.txt",
        ]
    );
    assert_eq!(
        parse_header_message(&account, "INBOX", 612, &[], raw)
            .expect("RFC 2231 header path")
            .subject,
        "Synthetic RFC 2231 pathologies"
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 612, &[], raw, "RFC 2231 preview".into(),)
            .expect("RFC 2231 catalogue path")
            .snippet,
        "RFC 2231 preview"
    );

    let fixture = fixture_imap_message(raw).expect("derive exact RFC 822 sections");
    let bodystructure = concat!(
        "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 40 1 NIL NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME*\" \"utf-8'et'caf%C3%A9.pdf\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 16 0 (\"ATTACHMENT\" (\"FILENAME\" \"unsupported-fallback.bin\" \"FILENAME*\" \"x-fictional'et'caf%C3%A9.bin\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME*2*\" \"tail.txt\" \"FILENAME*0*\" \"utf-8''ordered-\" \"FILENAME*1*\" \"middle-\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME\" \"missing-fallback.bin\" \"FILENAME*0*\" \"utf-8''missing-\" \"FILENAME*2*\" \"tail.bin\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME\" \"duplicate-fallback.bin\" \"FILENAME*0*\" \"utf-8''first-\" \"FILENAME*0*\" \"utf-8''second-\" \"FILENAME*1*\" \"tail.bin\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME*0\" \"long-\" \"FILENAME*1\" \"name.txt\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 8 0 (\"ATTACHMENT\" (\"FILENAME*0\" \"quo\\\"\" \"FILENAME*1\" \"te.txt\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 12 0 (\"ATTACHMENT\" (\"FILENAME*0\" \"back\\\\\" \"FILENAME*1\" \"slash.txt\")) NIL NIL) ",
        "(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 8 0 (\"ATTACHMENT\" (\"FILENAME*0*\" \"utf-8''mixed-quo\" \"FILENAME*1\" \"\\\"te.txt\")) NIL NIL) ",
        "\"MIXED\" (\"BOUNDARY\" \"rfc2231\") NIL NIL NIL)"
    );
    let structure =
        parse_bodystructure(&[format!("* 1 FETCH (UID 612 BODYSTRUCTURE {bodystructure})")])
            .expect("parse faithful RFC 2231 BODYSTRUCTURE");
    let response_lines =
        vec!["* 1 FETCH (UID 612 FLAGS () INTERNALDATE \"28-Jul-2026 10:22:00 +0000\")".into()];
    let (client_transport, server_transport) = duplex(MAX_RAW_MESSAGE_BYTES.min(1024 * 1024));
    let server = tokio::spawn(serve_fixture_selective_sections(
        server_transport,
        612,
        fixture.sections,
    ));
    let mut client = ImapClient {
        reader: BufReader::new(client_transport),
        tag: 0,
    };
    let selective = fetch_selective_message(
        &mut client,
        &account,
        "INBOX",
        612,
        &response_lines,
        &fixture.root_headers,
        &structure,
    )
    .await
    .expect("selective RFC 2231 fetch");
    drop(client);
    server
        .await
        .expect("selective fixture server task")
        .expect("serve selective fixture sections");
    assert_eq!(
        selective_user_visible_semantics(&selective),
        selective_user_visible_semantics(&complete),
        "complete and selective RFC 2231 recovery policy must agree"
    );
}

#[tokio::test]
async fn rfc2045_unknown_transfer_encoding_is_published_as_opaque_binary() {
    let raw = governed_raw("standards-unknown-transfer-encoding");
    let account = test_account();
    assert_eq!(
        parse_header_message(&account, "INBOX", 613, &[], raw)
            .expect("unknown transfer header path")
            .subject,
        "Synthetic unknown transfer encoding"
    );
    assert_eq!(
        parse_catalog_message(&account, "INBOX", 613, &[], raw, "visible preview".into(),)
            .expect("unknown transfer catalogue path")
            .snippet,
        "visible preview"
    );
    let parsed = parse_message(&account, "INBOX", 613, &[], raw, None)
        .await
        .expect("valid sibling body keeps message readable");
    assert_eq!(parsed.body_text.trim(), "Visible safe body.");
    assert!(parsed.body_html.is_none());
    assert_eq!(parsed.attachments.len(), 2);
    let opaque = &parsed.attachments[0];
    assert_eq!(opaque.attachment.filename, "invoice.exe");
    assert_eq!(opaque.attachment.mime_type, "application/octet-stream");
    assert!(opaque.attachment.is_potentially_unsafe);
    assert_eq!(
        opaque.bytes,
        b"Opaque transport bytes =QZ must not be decoded or discarded."
    );
    let unnamed = &parsed.attachments[1];
    assert_eq!(unnamed.attachment.filename, "attachment");
    assert_eq!(unnamed.attachment.mime_type, "application/octet-stream");
    assert_eq!(
        unnamed.bytes,
        b"<script>opaque HTML transport bytes must never render</script>"
    );

    let directory = tempfile::tempdir().expect("temporary fictional profile");
    let store = Store::open(directory.path().join("unknown-transfer.sqlite"))
        .await
        .expect("open fictional profile");
    let account = mime_recovery_account();
    store.save_account(&account).await.expect("save account");
    seed_mime_recovery_catalogue(&store, &account, &[613]).await;
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    let (reader_endpoint, reader_server) = scripted_unknown_transfer_server(raw, 613, None).await;
    let selective = service
        .fetch_message_at_epoch_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            613,
            MIME_RECOVERY_UID_VALIDITY,
            reader_endpoint,
        )
        .await
        .expect("fetch selective unknown-transfer metadata");
    let reader_transcript = reader_server.await.expect("Reader fixture server");
    assert!(reader_transcript.contains(&"UID FETCH 613 (BODY.PEEK[1])".into()));
    assert!(reader_transcript.contains(&"UID FETCH 613 (BODY.PEEK[2.MIME])".into()));
    assert!(reader_transcript.contains(&"UID FETCH 613 (BODY.PEEK[3.MIME])".into()));
    assert!(!reader_transcript.contains(&"UID FETCH 613 (BODY.PEEK[2])".into()));
    assert!(!reader_transcript.contains(&"UID FETCH 613 (BODY.PEEK[3])".into()));
    assert!(selective.body_html.is_none());
    assert_eq!(selective.attachments.len(), 2);
    let metadata = &selective.attachments[0].attachment;
    assert_eq!(metadata.filename, "invoice.exe");
    assert_eq!(metadata.mime_type, "application/octet-stream");
    assert_eq!(metadata.size_kind, AttachmentSizeKind::Encoded);
    assert!(metadata.is_potentially_unsafe);
    assert!(selective.attachments[0].bytes.is_empty());
    let unnamed_metadata = &selective.attachments[1].attachment;
    assert_eq!(unnamed_metadata.filename, "attachment");
    assert_eq!(unnamed_metadata.mime_type, "application/octet-stream");
    assert_eq!(unnamed_metadata.size_kind, AttachmentSizeKind::Encoded);
    assert!(selective.attachments[1].bytes.is_empty());

    let (attachment_endpoint, attachment_server) =
        scripted_unknown_transfer_server(raw, 613, None).await;
    let downloaded = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            613,
            &metadata.id,
            MIME_RECOVERY_UID_VALIDITY,
            attachment_endpoint,
        )
        .await
        .expect("target exact opaque attachment");
    let attachment_transcript = attachment_server.await.expect("attachment fixture server");
    assert!(attachment_transcript.contains(&"UID FETCH 613 (BODY.PEEK[2.MIME])".into()));
    assert!(attachment_transcript.contains(&"UID FETCH 613 (BODY.PEEK[2])".into()));
    assert_eq!(downloaded.attachment.id, metadata.id);
    assert_eq!(downloaded.attachment.filename, "invoice.exe");
    assert_eq!(downloaded.attachment.mime_type, "application/octet-stream");
    assert_eq!(downloaded.attachment.size_kind, AttachmentSizeKind::Decoded);
    assert!(downloaded.attachment.is_potentially_unsafe);
    assert_eq!(downloaded.bytes, opaque.bytes);
    assert_eq!(downloaded.attachment.size_bytes, opaque.bytes.len() as i64);

    let (unnamed_endpoint, unnamed_server) = scripted_unknown_transfer_server(raw, 613, None).await;
    let unnamed_downloaded = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            613,
            &unnamed_metadata.id,
            MIME_RECOVERY_UID_VALIDITY,
            unnamed_endpoint,
        )
        .await
        .expect("target exact unnamed opaque attachment");
    let unnamed_transcript = unnamed_server.await.expect("unnamed fixture server");
    assert!(unnamed_transcript.contains(&"UID FETCH 613 (BODY.PEEK[3.MIME])".into()));
    assert!(unnamed_transcript.contains(&"UID FETCH 613 (BODY.PEEK[3])".into()));
    assert_eq!(unnamed_downloaded.attachment.id, unnamed_metadata.id);
    assert_eq!(unnamed_downloaded.attachment.filename, "attachment");
    assert_eq!(
        unnamed_downloaded.attachment.mime_type,
        "application/octet-stream"
    );
    assert_eq!(
        unnamed_downloaded.attachment.size_kind,
        AttachmentSizeKind::Decoded
    );
    assert_eq!(unnamed_downloaded.bytes, unnamed.bytes);
    assert_eq!(
        unnamed_downloaded.attachment.size_bytes,
        unnamed.bytes.len() as i64
    );
}

#[tokio::test]
async fn targeted_save_rehydrates_attachment_omitted_by_bodystructure_before_matching_reader_id() {
    let raw = governed_raw("standards-bodystructure-omitted-attachment");
    let advertised_bodystructure = concat!(
        "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 19 1 NIL NIL NIL) ",
        "(\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 42 1 NIL NIL NIL) ",
        "\"MIXED\" (\"BOUNDARY\" \"omitted-attachment\") NIL NIL NIL)"
    )
    .to_owned();
    let directory = tempfile::tempdir().expect("temporary omitted-attachment profile");
    let store = Store::open(directory.path().join("omitted-attachment.sqlite"))
        .await
        .expect("open fictional profile");
    let account = mime_recovery_account();
    store.save_account(&account).await.expect("save account");
    seed_mime_recovery_catalogue(&store, &account, &[614]).await;
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    let (reader_endpoint, reader_server) =
        scripted_unknown_transfer_server(raw, 614, Some(advertised_bodystructure.clone())).await;
    let reader = service
        .fetch_message_at_epoch_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            614,
            MIME_RECOVERY_UID_VALIDITY,
            reader_endpoint,
        )
        .await
        .expect("Reader reclassifies raw MIME attachment headers");
    let reader_transcript = reader_server.await.expect("Reader fixture server");
    assert_eq!(reader.body_text.trim(), "Visible safe body.");
    assert!(reader.body_html.is_none());
    assert!(reader_transcript.contains(&"UID FETCH 614 (BODY.PEEK[2.MIME])".into()));
    assert!(!reader_transcript.contains(&"UID FETCH 614 (BODY.PEEK[2])".into()));
    assert_eq!(reader.attachments.len(), 1);
    let metadata = &reader.attachments[0].attachment;
    assert_eq!(
        metadata.id,
        stable_message_id(account.id, "INBOX", 614) + ":mime-v1:2"
    );
    assert_eq!(metadata.filename, "safe.html");
    assert_eq!(metadata.mime_type, "text/html");
    assert!(metadata.is_potentially_unsafe);
    assert!(reader.attachments[0].bytes.is_empty());

    let (save_endpoint, save_server) =
        scripted_unknown_transfer_server(raw, 614, Some(advertised_bodystructure)).await;
    let saved = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            614,
            &metadata.id,
            MIME_RECOVERY_UID_VALIDITY,
            save_endpoint,
        )
        .await
        .expect("targeted save rehydrates the same attachment classification");
    let save_transcript = save_server.await.expect("save fixture server");
    assert!(save_transcript.contains(&"UID FETCH 614 (BODY.PEEK[2.MIME])".into()));
    assert!(save_transcript.contains(&"UID FETCH 614 (BODY.PEEK[2])".into()));
    assert_eq!(saved.attachment.id, metadata.id);
    assert_eq!(saved.attachment.filename, "safe.html");
    assert_eq!(saved.attachment.mime_type, "text/html");
    assert!(saved.attachment.is_potentially_unsafe);
    assert_eq!(saved.attachment.size_kind, AttachmentSizeKind::Decoded);
    assert_eq!(saved.bytes, b"<p>Exact fictional attachment bytes.</p>");
    assert_eq!(saved.attachment.size_bytes, saved.bytes.len() as i64);
}

#[tokio::test]
async fn unknown_transfer_targeted_raw_bytes_obey_attachment_limit_at_the_exact_boundary() {
    let _large_buffer_guard = super::resource_budget_tests::LARGE_RESOURCE_TEST_LOCK
        .lock()
        .await;
    let structure = parse_bodystructure(&[concat!(
        "* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"HTML\" NIL NIL NIL ",
        "\"X-FICTIONAL-TRANSFER\" 1 0 NIL NIL NIL))"
    )
    .into()])
    .expect("parse unknown-transfer BODYSTRUCTURE");
    let plan = selective_plan(&structure).expect("plan opaque targeted attachment");
    assert_eq!(plan.attachments.len(), 1);
    assert!(plan.text_parts.is_empty());
    let headers = b"Content-Type: text/html; charset=us-ascii\r\nContent-Transfer-Encoding: x-fictional-transfer\r\n";

    let at_limit = attachment_data_from_part(
        &plan.attachments[0],
        "opaque-limit-message",
        headers,
        Some(vec![b'x'; MAX_ATTACHMENT_BYTES]),
        false,
    )
    .expect("opaque bytes at limit remain downloadable");
    assert_eq!(at_limit.attachment.mime_type, "application/octet-stream");
    assert_eq!(at_limit.bytes.len(), MAX_ATTACHMENT_BYTES);
    validate_targeted_attachment_bytes(&at_limit.bytes)
        .expect("opaque bytes at exact limit are accepted");
    drop(at_limit);

    let above_limit = attachment_data_from_part(
        &plan.attachments[0],
        "opaque-limit-message",
        headers,
        Some(vec![b'x'; MAX_ATTACHMENT_BYTES + 1]),
        false,
    )
    .expect("opaque extraction remains lossless before the targeted guard");
    assert_eq!(above_limit.bytes.len(), MAX_ATTACHMENT_BYTES + 1);
    assert_eq!(
        validate_targeted_attachment_bytes(&above_limit.bytes)
            .expect_err("opaque bytes above limit must be rejected")
            .to_string(),
        format!(
            "attachment exceeds the {} MiB safety limit",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        )
    );
}

#[tokio::test]
async fn mail_service_attachment_fetch_enforces_decoded_and_encoded_boundary_triads() {
    let _large_buffer_guard = super::resource_budget_tests::LARGE_RESOURCE_TEST_LOCK
        .lock()
        .await;
    let directory = tempfile::tempdir().expect("temporary attachment-budget profile");
    let database = directory.path().join("attachment-budget.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open attachment-budget profile");
    let account = mime_recovery_account();
    store.save_account(&account).await.expect("save account");
    let uids = [710, 711, 712, 713, 714];
    seed_mime_recovery_catalogue(&store, &account, &uids).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    let cases = [
        (
            710,
            LargeAttachmentBody::Raw {
                byte: b'x',
                bytes: MAX_ATTACHMENT_BYTES - 1,
            },
            Some((MAX_ATTACHMENT_BYTES - 1, b'x')),
            None,
        ),
        (
            711,
            LargeAttachmentBody::Base64Zeros {
                decoded: MAX_ATTACHMENT_BYTES,
                encoded: MAX_TARGETED_ATTACHMENT_ENCODED_BYTES - 1,
            },
            Some((MAX_ATTACHMENT_BYTES, 0)),
            None,
        ),
        (
            712,
            LargeAttachmentBody::Base64Zeros {
                decoded: MAX_ATTACHMENT_BYTES,
                encoded: MAX_TARGETED_ATTACHMENT_ENCODED_BYTES,
            },
            Some((MAX_ATTACHMENT_BYTES, 0)),
            None,
        ),
        (
            713,
            LargeAttachmentBody::Raw {
                byte: b'y',
                bytes: MAX_ATTACHMENT_BYTES + 1,
            },
            None,
            Some("attachment exceeds the 25 MiB safety limit"),
        ),
        (
            714,
            LargeAttachmentBody::AdvertiseOnly {
                bytes: MAX_TARGETED_ATTACHMENT_ENCODED_BYTES + 1,
            },
            None,
            Some("IMAP response literals exceed the 36 MiB safety limit"),
        ),
    ];

    for (uid, body, expected_success, expected_error) in cases {
        let (endpoint, server) = scripted_large_attachment_server(uid, body).await;
        let message_id = stable_message_id(account.id, "INBOX", uid);
        let attachment_id = format!("{message_id}:mime-v1:root");
        let result = service
            .fetch_attachment_at_epoch_with_endpoint_for_test(
                &account,
                "INBOX",
                uid,
                &attachment_id,
                MIME_RECOVERY_UID_VALIDITY,
                endpoint,
            )
            .await;
        match expected_success {
            Some((expected_len, expected_byte)) => {
                let attachment = result.expect("attachment boundary must be accepted");
                assert_eq!(attachment.attachment.id, attachment_id);
                assert_eq!(attachment.attachment.size_kind, AttachmentSizeKind::Decoded);
                assert_eq!(attachment.attachment.size_bytes, expected_len as i64);
                assert_eq!(attachment.bytes.len(), expected_len);
                assert!(attachment.bytes.iter().all(|byte| *byte == expected_byte));
            }
            None => assert_eq!(
                result
                    .expect_err("attachment boundary must be rejected")
                    .to_string(),
                expected_error.expect("rejected case error")
            ),
        }
        let transcript = server.await.expect("large attachment fixture server");
        assert!(transcript
            .commands
            .contains(&format!("UID FETCH {uid} (BODY.PEEK[TEXT])")));
        match body {
            LargeAttachmentBody::AdvertiseOnly { .. } => {
                assert_eq!(transcript.streamed_bytes, 0)
            }
            _ => assert_eq!(transcript.streamed_bytes, body.advertised_bytes()),
        }
        assert!(store
            .cached_message_content(&message_id)
            .await
            .expect("read attachment boundary cache state")
            .is_none());
        assert!(store
            .message(&message_id)
            .await
            .expect("read attachment boundary catalogue state")
            .is_some());
    }
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen attachment-budget profile");
    for uid in uids {
        let message_id = stable_message_id(account.id, "INBOX", uid);
        assert!(reopened
            .message(&message_id)
            .await
            .expect("read reopened attachment catalogue state")
            .is_some());
        assert!(reopened
            .cached_message_content(&message_id)
            .await
            .expect("read reopened attachment cache state")
            .is_none());
    }
}

#[tokio::test]
async fn mail_service_display_total_has_below_exact_and_above_recovery() {
    let _large_buffer_guard = super::resource_budget_tests::LARGE_RESOURCE_TEST_LOCK
        .lock()
        .await;
    let directory = tempfile::tempdir().expect("temporary display-budget profile");
    let database = directory.path().join("display-budget.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open display-budget profile");
    let account = mime_recovery_account();
    store.save_account(&account).await.expect("save account");
    let uids = [720, 721, 722];
    seed_mime_recovery_catalogue(&store, &account, &uids).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    for (uid, sizes) in [
        (
            720,
            vec![MAX_DISPLAY_PART_BYTES, MAX_DISPLAY_PART_BYTES - 1],
        ),
        (721, vec![MAX_DISPLAY_PART_BYTES, MAX_DISPLAY_PART_BYTES]),
    ] {
        let (endpoint, server) = scripted_large_display_server(uid, sizes.clone()).await;
        let message = service
            .hydrate_message_with_imap_endpoint_for_test(
                &account,
                "INBOX",
                uid,
                MIME_RECOVERY_UID_VALIDITY,
                endpoint,
            )
            .await
            .expect("display aggregate boundary must hydrate");
        let expected_len = sizes.iter().sum::<usize>() + 2;
        let cached = store
            .cached_message_content(&message.id)
            .await
            .expect("read display aggregate cache")
            .expect("accepted display aggregate must publish its cache");
        let body = cached.body_text.as_bytes();
        assert_eq!(body.len(), expected_len);
        assert!(body[..sizes[0]].iter().all(|byte| *byte == b'a'));
        assert_eq!(&body[sizes[0]..sizes[0] + 2], b"\n\n");
        assert!(body[sizes[0] + 2..].iter().all(|byte| *byte == b'b'));
        let transcript = server.await.expect("large display fixture server");
        assert_eq!(transcript.streamed_bytes, sizes.iter().sum::<usize>());
        assert!(transcript
            .commands
            .contains(&format!("UID FETCH {uid} (BODY.PEEK[1])")));
        assert!(transcript
            .commands
            .contains(&format!("UID FETCH {uid} (BODY.PEEK[2])")));
        drop(cached);
        drop(message);
    }

    let above_sizes = vec![MAX_DISPLAY_PART_BYTES, MAX_DISPLAY_PART_BYTES, 1];
    let (above_endpoint, above_server) =
        scripted_large_display_server(722, above_sizes.clone()).await;
    let error = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            722,
            MIME_RECOVERY_UID_VALIDITY,
            above_endpoint,
        )
        .await
        .expect_err("above-limit display aggregate must reject before the third body");
    assert_eq!(
        error.to_string(),
        "message display body exceeds the 50 MiB safety limit"
    );
    let above_transcript = above_server.await.expect("above display fixture server");
    assert_eq!(above_transcript.streamed_bytes, MAX_DISPLAY_TOTAL_BYTES);
    assert!(above_transcript
        .commands
        .contains(&"UID FETCH 722 (BODY.PEEK[1])".into()));
    assert!(above_transcript
        .commands
        .contains(&"UID FETCH 722 (BODY.PEEK[2])".into()));
    assert!(!above_transcript
        .commands
        .contains(&"UID FETCH 722 (BODY.PEEK[3])".into()));
    let rejected_id = stable_message_id(account.id, "INBOX", 722);
    assert!(store
        .cached_message_content(&rejected_id)
        .await
        .expect("read rejected display aggregate cache")
        .is_none());
    for accepted_uid in [720, 721] {
        assert!(store
            .cached_message_content(&stable_message_id(account.id, "INBOX", accepted_uid))
            .await
            .expect("read accepted cache after rejected sibling")
            .is_some());
    }
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen display-budget profile");
    for (uid, sizes) in [
        (720, [MAX_DISPLAY_PART_BYTES, MAX_DISPLAY_PART_BYTES - 1]),
        (721, [MAX_DISPLAY_PART_BYTES, MAX_DISPLAY_PART_BYTES]),
    ] {
        let cached = reopened
            .cached_message_content(&stable_message_id(account.id, "INBOX", uid))
            .await
            .expect("read reopened display aggregate cache")
            .expect("accepted display aggregate survives reopen");
        let body = cached.body_text.as_bytes();
        assert_eq!(body.len(), sizes.iter().sum::<usize>() + 2);
        assert!(body[..sizes[0]].iter().all(|byte| *byte == b'a'));
        assert_eq!(&body[sizes[0]..sizes[0] + 2], b"\n\n");
        assert!(body[sizes[0] + 2..].iter().all(|byte| *byte == b'b'));
        drop(cached);
    }
    assert!(reopened
        .cached_message_content(&rejected_id)
        .await
        .expect("read reopened rejected display aggregate cache")
        .is_none());
}

#[tokio::test]
async fn malformed_selective_body_does_not_disturb_cached_or_later_valid_reader_messages() {
    let directory = tempfile::tempdir().expect("temporary fictional profile");
    let database = directory.path().join("mime-reader-recovery.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let account = mime_recovery_account();
    store.save_account(&account).await.expect("save account");
    seed_mime_recovery_catalogue(&store, &account, &[41, 42, 43, 44]).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    let first_body = b"Valid body before malformed MIME.";
    let (first_endpoint, first_server) =
        scripted_mime_hydration_server(41, "7BIT", first_body, first_body.len()).await;
    let first = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            41,
            MIME_RECOVERY_UID_VALIDITY,
            first_endpoint,
        )
        .await
        .expect("hydrate first valid fictional message");
    let first_transcript = first_server.await.expect("first fictional server");
    assert!(
        first_transcript.contains(&"UID FETCH 41 (BODY.PEEK[TEXT])".into()),
        "Reader hydration omitted the text body fetch: {first_transcript:?}"
    );
    assert!(first_transcript.contains(&"LOGOUT".into()));
    assert_eq!(first.id, stable_message_id(account.id, "INBOX", 41));
    assert_eq!(
        store
            .cached_message_content(&first.id)
            .await
            .expect("read first hydrated cache")
            .expect("first hydration publishes Reader content")
            .body_text,
        "Valid body before malformed MIME.",
        "Reader cache after transcript {first_transcript:?}"
    );

    let malformed_body = b"not valid base64 !!!";
    let (malformed_endpoint, malformed_server) =
        scripted_mime_hydration_server(42, "BASE64", malformed_body, malformed_body.len()).await;
    let error = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            42,
            MIME_RECOVERY_UID_VALIDITY,
            malformed_endpoint,
        )
        .await
        .expect_err("invalid transfer bytes must reject Reader hydration");
    assert_eq!(error.to_string(), MIME_CONTENT_UNDECODABLE);
    let malformed_transcript = malformed_server.await.expect("malformed fictional server");
    assert!(malformed_transcript
        .iter()
        .any(|command| command == "UID FETCH 42 (BODY.PEEK[TEXT])"));
    assert!(!malformed_transcript.contains(&"LOGOUT".into()));
    assert!(store
        .cached_message_content(&stable_message_id(account.id, "INBOX", 42))
        .await
        .expect("read rejected cache state")
        .is_none());
    assert_eq!(
        store
            .cached_message_content(&first.id)
            .await
            .expect("read first cache after malformed sibling")
            .expect("first cache survives malformed sibling")
            .body_text,
        "Valid body before malformed MIME."
    );

    let (oversized_endpoint, oversized_server) =
        scripted_mime_hydration_server(44, "7BIT", b"", MAX_DISPLAY_PART_BYTES + 1).await;
    let oversized_error = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            44,
            MIME_RECOVERY_UID_VALIDITY,
            oversized_endpoint,
        )
        .await
        .expect_err("oversized advertised body must reject Reader hydration before fetching it");
    assert_eq!(
        oversized_error.to_string(),
        "message display part exceeds the 25 MiB safety limit"
    );
    let oversized_transcript = oversized_server.await.expect("oversized fictional server");
    assert!(oversized_transcript
        .iter()
        .any(|command| command.starts_with("UID FETCH 44 (FLAGS INTERNALDATE BODYSTRUCTURE")));
    assert!(!oversized_transcript.contains(&"UID FETCH 44 (BODY.PEEK[TEXT])".into()));
    assert!(store
        .cached_message_content(&stable_message_id(account.id, "INBOX", 44))
        .await
        .expect("read oversized rejected cache state")
        .is_none());
    assert_eq!(
        store
            .cached_message_content(&first.id)
            .await
            .expect("read first cache after oversized sibling")
            .expect("first cache survives oversized sibling")
            .body_text,
        "Valid body before malformed MIME."
    );

    let later_body = b"Valid body after malformed MIME.";
    let (later_endpoint, later_server) =
        scripted_mime_hydration_server(43, "7BIT", later_body, later_body.len()).await;
    let later = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            43,
            MIME_RECOVERY_UID_VALIDITY,
            later_endpoint,
        )
        .await
        .expect("hydrate later valid fictional message");
    assert_eq!(later.id, stable_message_id(account.id, "INBOX", 43));
    assert_eq!(
        store
            .cached_message_content(&later.id)
            .await
            .expect("read later hydrated cache")
            .expect("later hydration publishes Reader content")
            .body_text,
        "Valid body after malformed MIME."
    );
    later_server.await.expect("later fictional server");
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    assert_eq!(
        reopened
            .cached_message_content(&first.id)
            .await
            .expect("read first reopened cache")
            .expect("first cache remains after reopen")
            .body_text,
        "Valid body before malformed MIME."
    );
    assert!(reopened
        .cached_message_content(&stable_message_id(account.id, "INBOX", 42))
        .await
        .expect("read rejected reopened cache state")
        .is_none());
    assert!(reopened
        .cached_message_content(&stable_message_id(account.id, "INBOX", 44))
        .await
        .expect("read oversized rejected reopened cache state")
        .is_none());
    assert_eq!(
        reopened
            .cached_message_content(&later.id)
            .await
            .expect("read later reopened cache")
            .expect("later cache remains after reopen")
            .body_text,
        "Valid body after malformed MIME."
    );
}

#[tokio::test]
async fn ambiguous_inline_identities_survive_parse_reopen_and_reader_payload_handoff() {
    let handoff = reader_handoff();
    assert_eq!(handoff["fixtureId"], "ambiguous-inline-identities");

    let mut account = test_account();
    account.id = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111")
        .expect("fixture account UUID");
    let raw = include_bytes!("../../../testdata/mime/ambiguous-inline-identities.eml");
    let parsed = parse_message(&account, "INBOX", 42, &[], raw, None)
        .await
        .expect("the governed MIME corpus input must parse");

    assert_eq!(
        json!({
            "subject": parsed.subject,
            "snippet": parsed.snippet,
            "hasAttachments": parsed.has_attachments,
        }),
        handoff["readerMessage"],
        "the parsed mailbox row must preserve the declared Reader summary"
    );

    let directory = tempfile::tempdir().expect("temporary profile");
    let database = directory.path().join("mime-reader-handoff.sqlite");
    let store = Store::open(&database).await.expect("open profile");
    store.save_account(&account).await.expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&parsed))
        .await
        .expect("persist parsed message");
    // This is the same durable fallback consumed by the Tauri Reader command
    // when a message is not starred. It intentionally stores presentation
    // metadata, not attachment bytes.
    store
        .cache_message_content(
            &parsed.id,
            parsed.is_flagged,
            CachedMessageContent {
                body_text: parsed.body_text.clone(),
                body_html: parsed.body_html.clone(),
                unsubscribe_kind: parsed.unsubscribe_kind.clone(),
                attachments: parsed
                    .attachments
                    .iter()
                    .map(|attachment| attachment.attachment.clone())
                    .collect(),
            },
        )
        .await
        .expect("persist Reader content cache");
    drop(store);

    let reopened = Store::open(&database).await.expect("reopen profile");
    let persisted = reopened
        .message(&parsed.id)
        .await
        .expect("read persisted message")
        .expect("parsed message remains after reopen");
    let cached = reopened
        .cached_message_content(&parsed.id)
        .await
        .expect("read Reader content cache")
        .expect("complete parsed body remains available to Reader");

    let payload = json!({
        "body_text": cached.body_text,
        "body_html": cached.body_html,
        "unsubscribe_kind": cached.unsubscribe_kind.or(persisted.unsubscribe_kind),
        "attachments": cached.attachments.into_iter().filter(|attachment| attachment.presentation.is_downloadable()).map(|attachment| json!({
            "id": attachment.id,
            "message_id": attachment.message_id,
            "filename": attachment.filename,
            "mime_type": attachment.mime_type,
            "size_bytes": attachment.size_bytes,
            "is_inline": attachment.is_inline,
            "presentation": attachment.presentation,
            "is_potentially_unsafe": attachment.is_potentially_unsafe,
        })).collect::<Vec<_>>(),
    });
    assert_eq!(
        payload, handoff["messageContent"],
        "the reopened production Store projection must remain the checked-in Reader IPC payload"
    );
}

const PUBLIC_READER_FIXTURES: [&str; 13] = [
    "competing-nested-alternatives",
    "attached-message-rfc822",
    "multipart-attachment-container",
    "empty-alternative-leaf",
    "truncated-multipart-lf",
    "charset-matrix",
    "format-flowed-delsp",
    "international-reader-semantics",
    "ambiguous-inline-identities",
    "provider-signature-inline",
    "filename-parameters-and-encodings",
    "linkedin-inline-content-id",
    "standards-unknown-transfer-encoding",
];

fn downloadable_filenames(message: &MailSummary) -> Vec<String> {
    message
        .attachments
        .iter()
        .filter(|item| item.attachment.presentation.is_downloadable())
        .map(|item| item.attachment.filename.clone())
        .collect()
}

fn assert_governed_reader_semantics(name: &str, message: &MailSummary) {
    if name == "ambiguous-inline-identities" {
        let expected = reader_handoff();
        assert_eq!(
            message.subject,
            expected["readerMessage"]["subject"]
                .as_str()
                .expect("ambiguous corpus subject")
        );
        assert!(message.body_text.contains(
            expected["messageContent"]["body_text"]
                .as_str()
                .expect("ambiguous corpus text")
                .trim()
        ));
        assert!(message
            .body_html
            .as_deref()
            .is_some_and(|body| body.contains("cid:shared@example.test")));
        assert_eq!(downloadable_filenames(message), ["logo.png", "source.pdf"]);
        return;
    }

    if name == "provider-signature-inline" {
        let html = message
            .body_html
            .as_deref()
            .expect("provider signature keeps its selected HTML body");
        assert!(html.contains("Redacted message content for a fictional insurance case."));
        assert!(html.contains("data:image/png;base64,iVBORw0KGgo="));
        assert!(!html.contains("cid:image001.png@redacted"));
        assert_eq!(downloadable_filenames(message), ["claim-documents.pdf"]);
        assert_eq!(message.attachments.len(), 1);
        assert!(!message.attachments[0].attachment.is_inline);
        assert!(!message.attachments[0].attachment.is_potentially_unsafe);
        return;
    }

    if name == "filename-parameters-and-encodings" {
        assert_eq!(message.body_text.trim(), "Synthetic filename coverage.");
        assert!(message.body_html.is_none());
        let filenames = downloadable_filenames(message);
        assert_eq!(
            filenames,
            [
                "quarterly report final.pdf",
                "report-test.txt",
                "Prügimaja plaan vaated 31.08.pdf",
                "Prügimaja_31.08.pdf",
                "Prügimaja.pdf",
                "invoice;final.pdf",
                "invoicegpj.exe",
                "broken%ZZname.pdf",
            ]
        );
        assert!(filenames
            .iter()
            .all(|filename| !filename.contains(['/', '\\', '\u{202e}'])));
        let unsafe_attachments = message
            .attachments
            .iter()
            .filter(|attachment| attachment.attachment.is_potentially_unsafe)
            .map(|attachment| attachment.attachment.filename.as_str())
            .collect::<Vec<_>>();
        assert_eq!(unsafe_attachments, ["invoicegpj.exe"]);
        return;
    }

    if name == "linkedin-inline-content-id" {
        assert_eq!(
            message.body_text.trim(),
            "Redacted plain text=content with a foldedline."
        );
        assert!(message
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains("Redacted HTML=content with a foldedline.")));
        assert!(message.attachments.is_empty());
        return;
    }

    if name == "standards-unknown-transfer-encoding" {
        assert_eq!(message.body_text.trim(), "Visible safe body.");
        assert!(message.body_html.is_none());
        assert_eq!(
            downloadable_filenames(message),
            ["invoice.exe", "attachment"]
        );
        assert_eq!(message.attachments.len(), 2);
        assert_eq!(
            message
                .attachments
                .iter()
                .map(|attachment| attachment.attachment.mime_type.as_str())
                .collect::<Vec<_>>(),
            ["application/octet-stream", "application/octet-stream"]
        );
        assert!(message.attachments[0].attachment.is_potentially_unsafe);
        assert!(!message.attachments[1].attachment.is_potentially_unsafe);
        assert!(!message.body_text.contains("Opaque transport bytes"));
        assert!(!message.body_text.contains("<script>"));
        return;
    }

    let expected = &corpus_semantics()["scenarios"][name];
    for text in expected["bodyTextIncludes"]
        .as_array()
        .expect("declared Reader text inclusions")
    {
        assert!(
            message
                .body_text
                .contains(text.as_str().expect("Reader text")),
            "{name} loses declared Reader text {text}; subject={:?}; actual={:?}; html={:?}",
            message.subject,
            message.body_text,
            message.body_html
        );
    }
    for text in expected["bodyTextExcludes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        assert!(
            !message
                .body_text
                .contains(text.as_str().expect("Reader text")),
            "{name} leaks declared excluded Reader text {text}"
        );
    }
    for html in expected["bodyHtmlIncludes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        assert!(
            message
                .body_html
                .as_deref()
                .is_some_and(|body| body.contains(html.as_str().expect("Reader HTML"))),
            "{name} loses declared Reader HTML {html}"
        );
    }
    assert_eq!(
        downloadable_filenames(message),
        expected["downloadableFilenames"]
            .as_array()
            .expect("declared downloadable filenames")
            .iter()
            .map(|filename| filename.as_str().expect("filename").to_owned())
            .collect::<Vec<_>>(),
        "{name} attachment presentation"
    );
    if let Some(headers) = expected.get("headers") {
        assert_eq!(
            message.subject,
            headers["subject"].as_str().expect("declared subject")
        );
        assert_eq!(message.from_name.as_deref(), headers["fromName"].as_str());
        assert!(message
            .to_addresses
            .contains(headers["toAddressesInclude"].as_str().expect("recipient")));
    }
}

fn assert_truncated_reader_representation_tolerance(full: &MailSummary, selective: &MailSummary) {
    let expected = truncated_representation_tolerance();
    let reader_text = expected["readerText"]
        .as_str()
        .expect("declared Reader text");
    let forbidden = expected["forbiddenText"]
        .as_str()
        .expect("declared forbidden text");
    assert_eq!(
        full.body_text,
        expected["full"]["bodyText"]
            .as_str()
            .expect("declared complete body text")
    );
    assert_eq!(full.body_html, None);
    assert_eq!(
        selective.body_text,
        expected["selective"]["bodyText"]
            .as_str()
            .expect("declared selective body text")
    );
    assert_eq!(
        selective.body_html.as_deref(),
        expected["selective"]["bodyHtml"].as_str()
    );
    for message in [full, selective] {
        assert!(
            message.body_text.contains(reader_text)
                || message
                    .body_html
                    .as_deref()
                    .is_some_and(|html| html.contains(reader_text)),
            "every allowed representation must retain the declared Reader text"
        );
        assert!(!message.body_text.contains(forbidden));
        assert!(!message
            .body_html
            .as_deref()
            .is_some_and(|html| html.contains(forbidden)));
        assert_eq!(
            downloadable_filenames(message),
            expected["full"]["downloadableFilenames"]
                .as_array()
                .expect("declared attachment presentation")
                .iter()
                .map(|value| value.as_str().expect("filename").to_owned())
                .collect::<Vec<_>>()
        );
        assert!(
            message.attachments.is_empty(),
            "the malformed-boundary tolerance allows no attachment metadata"
        );
    }
}

async fn scripted_full_message_server(
    raw: &'static [u8],
    uid: u32,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional full-message IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional full-message IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional full-message command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            transcript.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"mime secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 1 EXISTS\r\n* OK [UIDVALIDITY {MIME_RECOVERY_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if matches!(
                command,
                _ if command == format!("UID FETCH {uid} (FLAGS INTERNALDATE BODY.PEEK[])")
                    || command == format!("UID FETCH {uid} (BODY.PEEK[])")
            ) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 10:23:00 +0000\" BODY[]<0> {{{}}}\r\n",
                            raw.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write full-message literal prefix");
                write
                    .write_all(raw)
                    .await
                    .expect("write full-message bytes");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK full message\r\n").as_bytes())
                    .await
                    .expect("finish full-message response");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional full-message command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

async fn seed_mime_catalogue_from_raw(
    store: &Store,
    account: &Account,
    uid: u32,
    raw: &[u8],
) -> MailSummary {
    store
        .save_mailbox_catalog_state(
            account.id,
            "INBOX",
            "INBOX",
            MIME_RECOVERY_UID_VALIDITY as u32,
            2,
            true,
        )
        .await
        .expect("save fictional mailbox state");
    let catalogue_response = [format!(
        "* 1 FETCH (UID {uid} INTERNALDATE \"28-Jul-2026 10:23:00 +0000\")"
    )];
    let mut message = parse_catalog_message(
        account,
        "INBOX",
        uid,
        &catalogue_response,
        raw,
        "catalogue preview".into(),
    )
    .expect("parse governed catalogue metadata");
    message.mailbox_uid_validity = Some(MIME_RECOVERY_UID_VALIDITY);
    store
        .upsert_messages(std::slice::from_ref(&message))
        .await
        .expect("save governed catalogue row");
    message
}

#[tokio::test]
async fn greenmail_mime_topology_contradiction_falls_back_to_bounded_raw_without_publishing_fake_attachments(
) {
    let raw = governed_raw("greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction");
    let transcript: Value = serde_json::from_str(include_str!(
        "../../../../../testdata/mime-reader-handoff/greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction.json"
    ))
    .expect("captured GreenMail transcript must remain valid JSON");
    assert_eq!(
        transcript["protocol"],
        json!([
            "EXAMINE INBOX",
            "STATUS INBOX (UIDNEXT)",
            "UID FETCH 6 (FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)])",
            "UID FETCH 6 (BODY.PEEK[1.MIME])",
            "UID FETCH 6 (BODY.PEEK[1.1.MIME])",
            "UID FETCH 6 (BODY.PEEK[1.2.MIME])",
            "UID FETCH 6 (BODY.PEEK[1.1])",
            "UID FETCH 6 (BODY.PEEK[1.2])",
        ]),
        "the source evidence must retain the observed selective command sequence"
    );
    let captured_metadata = STANDARD
        .decode(
            transcript["captures"][0]["items"][0]["literalBase64"]
                .as_str()
                .expect("captured metadata literal"),
        )
        .expect("captured metadata base64");
    assert_eq!(captured_metadata, GREENMAIL_UID6_METADATA_HEADERS);
    let captured_bodystructure = STANDARD
        .decode(
            transcript["captures"][0]["items"][0]["metadataBase64"]
                .as_str()
                .expect("captured BODYSTRUCTURE metadata"),
        )
        .expect("captured BODYSTRUCTURE base64");
    assert!(
        String::from_utf8_lossy(&captured_bodystructure).contains(GREENMAIL_UID6_BODYSTRUCTURE),
        "the scripted TLS replay must retain the captured nested BODYSTRUCTURE"
    );
    for index in 1..=3 {
        let headers = STANDARD
            .decode(
                transcript["captures"][index]["items"][0]["literalBase64"]
                    .as_str()
                    .expect("captured MIME header literal"),
            )
            .expect("captured MIME header base64");
        assert_eq!(
            headers, GREENMAIL_UID6_REPEATED_PARENT_HEADERS,
            "capture {index} must retain the repeated parent MIME headers"
        );
    }

    let directory = tempfile::tempdir().expect("temporary GreenMail topology profile");
    let database = directory.path().join("greenmail-topology.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let account = mime_recovery_account();
    store
        .save_account(&account)
        .await
        .expect("save fictional account");
    let catalogue = seed_mime_catalogue_from_raw(&store, &account, 6, raw).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");

    let (endpoint, server) =
        scripted_greenmail_uid6_topology_contradiction_server(ContradictoryRaw::Present(raw)).await;
    let hydrated = service
        .hydrate_message_with_imap_endpoint_for_test(
            &account,
            "INBOX",
            6,
            MIME_RECOVERY_UID_VALIDITY,
            endpoint,
        )
        .await
        .expect("contradictory selective headers recover through one raw fallback");
    let commands = server.await.expect("GreenMail topology fixture server");

    assert!(
        hydrated.attachments.is_empty(),
        "no fake multipart attachments"
    );
    assert!(
        hydrated.message_id.is_none(),
        "incoming absent Message-ID stays absent"
    );
    assert!(commands.contains(&"UID FETCH 6 (BODY.PEEK[1.MIME])".into()));
    assert!(commands.contains(&"UID FETCH 6 (BODY.PEEK[1.1.MIME])".into()));
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.as_str() == "UID FETCH 6 (FLAGS INTERNALDATE BODY.PEEK[])")
            .count(),
        1,
        "one same-UID bounded fallback only"
    );
    assert!(
        !commands.iter().any(|command| {
            command.contains("BODY.PEEK[1.2")
                || command == "UID FETCH 6 (BODY.PEEK[1.1])"
                || command.contains(" STORE ")
        }),
        "the contradiction stops selective leaf reads and remains read-neutral: {commands:?}"
    );
    let cached = store
        .cached_message_content(&catalogue.id)
        .await
        .expect("read published fallback content")
        .expect("successful fallback publishes complete Reader content");
    assert_eq!(
        cached.body_text.trim(),
        "One isolated fictional message for native send outcome acceptance. Do not resend."
    );
    assert!(cached
        .body_html
        .as_deref()
        .is_some_and(|html| html.contains("One isolated fictional message")));
    assert!(cached.attachments.is_empty());
}

#[tokio::test]
async fn greenmail_mime_topology_fallback_errors_never_publish_partial_content() {
    let catalogue_raw = governed_raw("greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction");
    let malformed_raw = include_bytes!("../../../testdata/mime/fuzz-invalid-part-id-minimized.eml");
    for (name, raw, expected_error) in [
        ("missing", ContradictoryRaw::Missing, None),
        (
            "malformed",
            ContradictoryRaw::Present(malformed_raw),
            Some(MIME_CONTENT_UNDECODABLE),
        ),
        ("oversized", ContradictoryRaw::Oversized, None),
    ] {
        let directory = tempfile::tempdir().expect("temporary GreenMail fallback failure profile");
        let database = directory
            .path()
            .join(format!("greenmail-topology-{name}.sqlite"));
        let store = Store::open(&database)
            .await
            .expect("open fictional profile");
        let account = mime_recovery_account();
        store
            .save_account(&account)
            .await
            .expect("save fictional account");
        let catalogue = seed_mime_catalogue_from_raw(&store, &account, 6, catalogue_raw).await;
        let service = MailService::new(store.clone());
        service
            .credentials()
            .set_password(&account, "mime secret")
            .await
            .expect("save fictional credential");

        let (endpoint, server) = scripted_greenmail_uid6_topology_contradiction_server(raw).await;
        let error = service
            .hydrate_message_with_imap_endpoint_for_test(
                &account,
                "INBOX",
                6,
                MIME_RECOVERY_UID_VALIDITY,
                endpoint,
            )
            .await
            .expect_err("fallback {name} must not publish a partial Reader message");
        if let Some(expected_error) = expected_error {
            assert_eq!(error.to_string(), expected_error, "{name}");
        }
        let commands = server
            .await
            .expect("GreenMail fallback failure fixture server");
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.as_str() == "UID FETCH 6 (FLAGS INTERNALDATE BODY.PEEK[])")
                .count(),
            1,
            "{name} receives no retry"
        );
        assert!(
            !commands.iter().any(|command| command.contains(" STORE ")),
            "{name} failure must stay read-neutral: {commands:?}"
        );
        assert!(
            store
                .cached_message_content(&catalogue.id)
                .await
                .expect("read failed fallback cache")
                .is_none(),
            "{name} fallback must not publish partial content"
        );
        assert_eq!(
            store
                .message(&catalogue.id)
                .await
                .expect("read failed fallback catalogue")
                .expect("failure retains catalogue row")
                .content_state,
            catalogue.content_state,
            "{name} fallback must not mutate catalogue metadata"
        );
    }
}

const READER_ARTIFACT_MARKER: &str = ".dakia-mime-reader-artifact";
const READER_ARTIFACT_PROFILE: &str = "mime-reader-profile.sqlite";

fn checked_reader_artifact_root(nonce: &str) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("DAKIA_MIME_READER_CORE_ARTIFACT_ROOT")?);
    assert!(root.is_absolute(), "artifact root must be absolute");
    let root = root
        .canonicalize()
        .expect("artifact root must already exist");
    let marker_path = root.join(READER_ARTIFACT_MARKER);
    let marker =
        std::fs::read_to_string(&marker_path).expect("artifact root must have an ownership marker");
    assert_eq!(marker, format!("{nonce}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root_metadata = std::fs::metadata(&root).expect("artifact root metadata");
        assert!(root_metadata.is_dir(), "artifact root must be a directory");
        assert_eq!(root_metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(root_metadata.permissions().mode() & 0o777, 0o700);
        let marker_metadata =
            std::fs::symlink_metadata(&marker_path).expect("artifact ownership marker metadata");
        assert!(marker_metadata.file_type().is_file());
        assert_eq!(marker_metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(marker_metadata.permissions().mode() & 0o777, 0o600);
    }
    assert!(
        !root.join(READER_ARTIFACT_PROFILE).exists(),
        "artifact export refuses to overwrite a profile"
    );
    assert!(
        !root.join("core-manifest.json").exists(),
        "artifact export refuses to overwrite a manifest"
    );
    Some(root)
}

#[tokio::test]
async fn public_full_and_selective_reader_fetches_preserve_every_governed_reader_fixture() {
    let nonce = std::env::var("DAKIA_MIME_READER_ARTIFACT_NONCE")
        .unwrap_or_else(|_| "standalone-provider-reader-verification".into());
    let temporary;
    let external = checked_reader_artifact_root(&nonce);
    let root = if let Some(root) = external.as_ref() {
        root.clone()
    } else {
        temporary = tempfile::tempdir().expect("temporary public Reader profile");
        temporary.path().to_path_buf()
    };
    let database = root.join(READER_ARTIFACT_PROFILE);
    let store = Store::open(&database)
        .await
        .expect("open shared public Reader profile");
    let mut account = mime_recovery_account();
    account.id = uuid::Uuid::parse_str("13131313-1313-4313-8313-131313131313")
        .expect("fixed fictional artifact account UUID");
    store
        .save_account(&account)
        .await
        .expect("save fictional artifact account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "mime secret")
        .await
        .expect("save fictional credential");
    let mut exported = Vec::new();
    let mut expected_selective = Vec::new();

    for (offset, name) in PUBLIC_READER_FIXTURES.into_iter().enumerate() {
        let uid = 800 + offset as u32;
        let raw = governed_raw(name);
        let catalogue = seed_mime_catalogue_from_raw(&store, &account, uid, raw).await;
        assert!(
            store
                .cached_message_content(&catalogue.id)
                .await
                .expect("read unhydrated Reader cache")
                .is_none(),
            "{name} must begin as an unhydrated catalogue row"
        );

        if offset == 0 {
            let error = service
                .hydrate_message_with_imap_endpoint_for_test(
                    &account,
                    "INBOX",
                    uid,
                    MIME_RECOVERY_UID_VALIDITY + 1,
                    imap_test_endpoint(9, true),
                )
                .await
                .expect_err("a stale captured epoch must fail before provider access");
            assert!(
                error.to_string().contains("mailbox identity changed"),
                "stale profile sensitivity must fail at the UIDVALIDITY fence: {error}"
            );
            assert!(
                store
                    .cached_message_content(&catalogue.id)
                    .await
                    .expect("read stale-attempt cache")
                    .is_none(),
                "a stale hydration attempt must not populate Reader content"
            );
        }

        let (full_endpoint, full_server) = scripted_full_message_server(raw, uid).await;
        let full = service
            .fetch_full_message_with_imap_endpoint_for_test(&account, "INBOX", uid, full_endpoint)
            .await
            .unwrap_or_else(|error| panic!("{name} public full fetch failed: {error}"));
        let full_transcript = full_server.await.expect("full Reader fixture server");
        assert!(
            full_transcript.contains(&format!("UID FETCH {uid} (FLAGS INTERNALDATE BODY.PEEK[])")),
            "{name} full fetch was not explicitly read-neutral: {full_transcript:?}"
        );
        assert!(
            !full_transcript
                .iter()
                .any(|command| command.contains(" BODY[]")
                    || command == &format!("UID FETCH {uid} (BODY[])")),
            "{name} full fetch issued a Seen-changing full-body command: {full_transcript:?}"
        );
        assert_governed_reader_semantics(name, &full);

        let (selective_endpoint, selective_server) =
            scripted_unknown_transfer_server(raw, uid, None).await;
        let selective = service
            .fetch_message_at_epoch_with_imap_endpoint_for_test(
                &account,
                "INBOX",
                uid,
                MIME_RECOVERY_UID_VALIDITY,
                selective_endpoint,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} public selective Reader fetch failed: {error}"));
        let selective_transcript = selective_server
            .await
            .expect("selective Reader fixture server");
        assert!(
            selective_transcript
                .iter()
                .any(|command| command.starts_with(&format!(
                    "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
                ))),
            "{name} selective fetch omitted metadata: {selective_transcript:?}"
        );
        assert!(
            !selective_transcript.iter().any(|command| command.contains("BODY.PEEK[]")),
            "{name} selective Reader fetch retrieved the complete message: {selective_transcript:?}"
        );
        if name == "truncated-multipart-lf" {
            assert_truncated_reader_representation_tolerance(&full, &selective);
        } else {
            assert_governed_reader_semantics(name, &selective);
            assert_eq!(
                selective_user_visible_semantics(&selective),
                selective_user_visible_semantics(&full),
                "{name} complete and selective public Reader paths diverged"
            );
        }

        let (hydration_endpoint, hydration_server) =
            scripted_unknown_transfer_server(raw, uid, None).await;
        let published = service
            .hydrate_message_with_imap_endpoint_for_test(
                &account,
                "INBOX",
                uid,
                MIME_RECOVERY_UID_VALIDITY,
                hydration_endpoint,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} public Reader publication failed: {error}"));
        let hydration_transcript = hydration_server
            .await
            .expect("Reader publication fixture server");
        assert_eq!(
            published.id, catalogue.id,
            "{name} publication changed identity"
        );
        assert!(
            !hydration_transcript
                .iter()
                .any(|command| command.contains("BODY.PEEK[]")),
            "{name} Reader publication retrieved the complete message: {hydration_transcript:?}"
        );
        expected_selective.push((name.to_owned(), selective));
        exported.push(json!({
            "fixtureId": name,
            "messageId": catalogue.id,
            "expectedUidValidity": MIME_RECOVERY_UID_VALIDITY,
        }));
    }
    store
        .save_mailbox_catalog_state(
            account.id,
            "INBOX",
            "INBOX",
            MIME_RECOVERY_UID_VALIDITY as u32,
            PUBLIC_READER_FIXTURES.len(),
            true,
        )
        .await
        .expect("save faithful shared mailbox total");

    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen the exact shared hydrated Reader profile");
    for entry in &exported {
        let name = entry["fixtureId"].as_str().expect("fixture identifier");
        let message_id = entry["messageId"].as_str().expect("message identifier");
        let reopened_summary = reopened
            .message(message_id)
            .await
            .expect("read reopened summary")
            .expect("Reader publication keeps catalogue row");
        let reopened_content = reopened
            .cached_message_content(message_id)
            .await
            .expect("read reopened Reader cache")
            .expect("Reader publication caches selected content");
        assert_eq!(
            reopened_summary.mailbox_uid_validity,
            Some(MIME_RECOVERY_UID_VALIDITY),
            "{name} mailbox epoch survives reopen"
        );
        let expected = expected_selective
            .iter()
            .find_map(|(fixture, message)| (fixture == name).then_some(message))
            .expect("retain exact selective projection");
        assert_eq!(
            reopened_content.body_text, expected.body_text,
            "{name} exact body text survives reopen"
        );
        assert_eq!(
            reopened_content.body_html, expected.body_html,
            "{name} exact HTML survives reopen"
        );
        assert_eq!(
            serde_json::to_value(&reopened_content.attachments)
                .expect("serialize reopened attachment metadata"),
            serde_json::to_value(
                expected
                    .attachments
                    .iter()
                    .map(|item| item.attachment.clone())
                    .collect::<Vec<_>>()
            )
            .expect("serialize expected selective attachment metadata"),
            "{name} complete attachment safety metadata survives reopen"
        );
        let reopened_projection = MailSummary {
            body_text: reopened_content.body_text.clone(),
            body_html: reopened_content.body_html.clone(),
            attachments: reopened_content
                .attachments
                .iter()
                .cloned()
                .map(|attachment| AttachmentData {
                    attachment,
                    bytes: Vec::new(),
                })
                .collect(),
            ..reopened_summary
        };
        if name == "truncated-multipart-lf" {
            let expected = truncated_representation_tolerance();
            assert_eq!(
                reopened_projection.body_text,
                expected["selective"]["bodyText"]
                    .as_str()
                    .expect("declared selective body text")
            );
            assert_eq!(
                reopened_projection.body_html.as_deref(),
                expected["selective"]["bodyHtml"].as_str()
            );
            assert!(reopened_projection.attachments.is_empty());
        } else {
            assert_governed_reader_semantics(name, &reopened_projection);
        }
    }
    let catalog_state = reopened
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .expect("read reopened mailbox state")
        .expect("shared mailbox state survives reopen");
    assert_eq!(catalog_state.uid_validity, MIME_RECOVERY_UID_VALIDITY);
    assert_eq!(
        catalog_state.remote_total,
        PUBLIC_READER_FIXTURES.len() as i64,
        "shared profile total must match its persisted messages"
    );
    drop(reopened);

    let checkpoint_url = format!("sqlite://{}", database.display());
    let checkpoint_pool = sqlx::SqlitePool::connect(&checkpoint_url)
        .await
        .expect("open hydrated profile for a coherent WAL checkpoint");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&checkpoint_pool)
        .await
        .expect("checkpoint hydrated Reader profile");
    checkpoint_pool.close().await;
    let wal = database.with_extension("sqlite-wal");
    assert!(
        !wal.exists() || std::fs::metadata(&wal).expect("WAL metadata").len() == 0,
        "artifact profile must be checkpointed before handoff"
    );

    if external.is_some() {
        let manifest = json!({
            "schemaVersion": 1,
            "nonce": nonce,
            "database": READER_ARTIFACT_PROFILE,
            "producer": "MailService::hydrate_message_with_imap_endpoint_for_test",
            "profileState": "closed-and-reopened-after-provider-hydration",
            "messages": exported,
        });
        std::fs::write(
            root.join("core-manifest.json"),
            serde_json::to_vec_pretty(&manifest).expect("serialize core artifact manifest"),
        )
        .expect("write core artifact manifest");
    }
}

#[tokio::test]
async fn public_full_fetch_rejects_retained_malformed_seeds_without_disturbing_neighbor_cache() {
    let cases: [(&str, &'static [u8]); 2] = [
        (
            "minimized",
            include_bytes!("../../../testdata/mime/fuzz-invalid-part-id-minimized.eml"),
        ),
        (
            "original",
            include_bytes!("../../../testdata/mime/fuzz-invalid-part-id-original.eml"),
        ),
    ];
    for (offset, (seed_name, raw)) in cases.into_iter().enumerate() {
        let uid = 902 + u32::try_from(offset * 2).expect("fixture UID");
        let account = mime_recovery_account();
        let directory = tempfile::tempdir().expect("temporary malformed full-fetch profile");
        let database = directory.path().join("malformed-full-fetch.sqlite");
        let store = Store::open(&database)
            .await
            .expect("open malformed full-fetch profile");
        store
            .save_account(&account)
            .await
            .expect("save fictional account");
        let malformed = seed_mime_catalogue_from_raw(&store, &account, uid, raw).await;
        let mut neighbor = parse_message(
            &account,
            "INBOX",
            uid + 1,
            &[],
            governed_raw("standards-base64-final-quanta"),
            None,
        )
        .await
        .expect("parse valid neighboring Reader message");
        neighbor.mailbox_uid_validity = Some(MIME_RECOVERY_UID_VALIDITY);
        store
            .upsert_messages(std::slice::from_ref(&neighbor))
            .await
            .expect("save valid neighboring summary");
        store
            .cache_message_content(
                &neighbor.id,
                neighbor.is_flagged,
                CachedMessageContent {
                    body_text: neighbor.body_text.clone(),
                    body_html: neighbor.body_html.clone(),
                    unsubscribe_kind: neighbor.unsubscribe_kind.clone(),
                    attachments: neighbor
                        .attachments
                        .iter()
                        .map(|item| item.attachment.clone())
                        .collect(),
                },
            )
            .await
            .expect("cache valid neighboring Reader content");
        let service = MailService::new(store.clone());
        service
            .credentials()
            .set_password(&account, "mime secret")
            .await
            .expect("save fictional credential");

        let (endpoint, server) = scripted_full_message_server(raw, uid).await;
        let error = service
            .fetch_full_message_with_imap_endpoint_for_test(&account, "INBOX", uid, endpoint)
            .await
            .expect_err("retained fuzz seed must reject public full Reader fetch");
        assert_eq!(error.to_string(), MIME_CONTENT_UNDECODABLE, "{seed_name}");
        let transcript = server.await.expect("malformed full-fetch fixture server");
        assert!(transcript.contains(&format!("UID FETCH {uid} (FLAGS INTERNALDATE BODY.PEEK[])")));
        assert!(
            !transcript.iter().any(|command| command.contains(" BODY[]")
                || command == &format!("UID FETCH {uid} (BODY[])")),
            "{seed_name} failed full fetch must stay read-neutral: {transcript:?}"
        );

        drop(service);
        drop(store);
        let reopened = Store::open(&database)
            .await
            .expect("reopen malformed full-fetch profile");
        let retained = reopened
            .message(&malformed.id)
            .await
            .expect("read retained malformed metadata")
            .expect("malformed body cannot remove catalogue row");
        assert_eq!(retained.subject, malformed.subject, "{seed_name}");
        assert!(
            reopened
                .cached_message_content(&malformed.id)
                .await
                .expect("read malformed cache")
                .is_none(),
            "{seed_name} failed full fetch must not publish partial Reader content"
        );
        assert_eq!(
            reopened
                .cached_message_content(&neighbor.id)
                .await
                .expect("read neighboring cache")
                .expect("neighbor cache survives malformed full fetch")
                .body_text,
            neighbor.body_text,
            "{seed_name} must not disturb the neighboring cache"
        );
    }
}

#[tokio::test]
async fn public_full_fetch_rejects_invalid_part_graph_seeds_without_publishing_content_or_disturbing_neighbor_cache(
) {
    let cases: [(&str, &'static [u8]); 2] = [
        (
            "minimized",
            include_bytes!("../../../testdata/mime/fuzz-budget-graph-oom-minimized.eml"),
        ),
        (
            "original",
            include_bytes!("../../../testdata/mime/fuzz-budget-graph-oom-original.eml"),
        ),
    ];
    for (offset, (seed_name, raw)) in cases.into_iter().enumerate() {
        let uid = 906 + u32::try_from(offset * 2).expect("fixture UID");
        let account = mime_recovery_account();
        let directory = tempfile::tempdir().expect("temporary malformed graph full-fetch profile");
        let database = directory.path().join("malformed-graph-full-fetch.sqlite");
        let store = Store::open(&database)
            .await
            .expect("open malformed graph full-fetch profile");
        store
            .save_account(&account)
            .await
            .expect("save fictional account");
        // These malformed graph seeds are complete-parser findings. Do not
        // manufacture a catalogue row from their corrupted raw headers merely
        // to drive full fetch: selection needs only durable mailbox state.
        seed_mime_recovery_catalogue(&store, &account, &[uid + 1]).await;
        let mut neighbor = parse_message(
            &account,
            "INBOX",
            uid + 1,
            &[],
            governed_raw("standards-base64-final-quanta"),
            None,
        )
        .await
        .expect("parse valid neighboring Reader message");
        neighbor.mailbox_uid_validity = Some(MIME_RECOVERY_UID_VALIDITY);
        store
            .upsert_messages(std::slice::from_ref(&neighbor))
            .await
            .expect("save valid neighboring summary");
        store
            .cache_message_content(
                &neighbor.id,
                neighbor.is_flagged,
                CachedMessageContent {
                    body_text: neighbor.body_text.clone(),
                    body_html: neighbor.body_html.clone(),
                    unsubscribe_kind: neighbor.unsubscribe_kind.clone(),
                    attachments: neighbor
                        .attachments
                        .iter()
                        .map(|item| item.attachment.clone())
                        .collect(),
                },
            )
            .await
            .expect("cache valid neighboring Reader content");
        let service = MailService::new(store.clone());
        service
            .credentials()
            .set_password(&account, "mime secret")
            .await
            .expect("save fictional credential");

        let (endpoint, server) = scripted_full_message_server(raw, uid).await;
        let error = service
            .fetch_full_message_with_imap_endpoint_for_test(&account, "INBOX", uid, endpoint)
            .await
            .expect_err("invalid graph seed must reject public complete-message fetch");
        assert_eq!(error.to_string(), "mime_invalid_part_graph", "{seed_name}");
        let transcript = server
            .await
            .expect("malformed graph full-fetch fixture server");
        assert!(transcript.contains(&format!("UID FETCH {uid} (FLAGS INTERNALDATE BODY.PEEK[])")));
        assert!(
            !transcript.iter().any(|command| command.contains(" BODY[]")
                || command == &format!("UID FETCH {uid} (BODY[])")),
            "{seed_name} failed full fetch must stay read-neutral: {transcript:?}"
        );

        drop(service);
        drop(store);
        let reopened = Store::open(&database)
            .await
            .expect("reopen malformed graph full-fetch profile");
        let malformed_id = stable_message_id(account.id, "INBOX", uid);
        assert!(
            reopened
                .message(&malformed_id)
                .await
                .expect("read malformed graph catalogue row")
                .is_none(),
            "{seed_name} failure must not invent a catalogue row"
        );
        assert!(
            reopened
                .cached_message_content(&malformed_id)
                .await
                .expect("read malformed graph cache")
                .is_none(),
            "{seed_name} failed full fetch must not publish body or attachment cache data"
        );
        assert_eq!(
            reopened
                .cached_message_content(&neighbor.id)
                .await
                .expect("read neighboring cache")
                .expect("neighbor cache survives malformed graph full fetch")
                .body_text,
            neighbor.body_text,
            "{seed_name} must not disturb the neighboring cache"
        );
    }
}
