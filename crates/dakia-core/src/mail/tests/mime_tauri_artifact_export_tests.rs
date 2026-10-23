use super::*;
use crate::CachedMessageContent;
use serde_json::json;
use std::path::{Path, PathBuf};

const ARTIFACT_MARKER: &str = ".dakia-mime-reader-artifact";
const PROFILE_NAME: &str = "mime-reader-profile.sqlite";
const UID_VALIDITY: i64 = 7_707;

const GOVERNED_READER_FIXTURES: [&str; 9] = [
    // These are the corpus cases with declared Reader semantics. Keep the
    // artifact lane focused on complete, parseable messages rather than
    // treating every parser regression as a Reader acceptance case.
    "competing-nested-alternatives",
    "attached-message-rfc822",
    "multipart-attachment-container",
    "empty-alternative-leaf",
    "truncated-multipart-lf",
    "charset-matrix",
    "format-flowed-delsp",
    "international-reader-semantics",
    "ambiguous-inline-identities",
];

fn governed_fixture(name: &str) -> &'static [u8] {
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
        "format-flowed-delsp" => {
            include_bytes!("../../../testdata/mime/format-flowed-delsp.eml")
        }
        "ambiguous-inline-identities" => {
            include_bytes!("../../../testdata/mime/ambiguous-inline-identities.eml")
        }
        "international-reader-semantics" => {
            include_bytes!("../../../testdata/mime/international-reader-semantics.eml")
        }
        other => panic!("unknown governed MIME artifact fixture: {other}"),
    }
}

fn assert_reader_semantics(
    fixture_id: &str,
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachment_filenames: &[String],
) {
    match fixture_id {
        "competing-nested-alternatives" => {
            assert!(body_text.contains("Visible plain body."));
            assert!(!body_text.contains("redacted-notes"));
            assert!(
                body_html.is_some_and(|body| body.contains("Visible <strong>HTML</strong> body."))
            );
            assert_eq!(attachment_filenames, ["redacted-notes.txt"]);
        }
        "attached-message-rfc822" => {
            assert!(body_text.contains("fully redacted"));
            assert!(!body_text.contains("Original plain text"));
            assert_eq!(body_html, None);
            assert_eq!(attachment_filenames, ["forwarded-message.eml"]);
        }
        "multipart-attachment-container" => {
            assert!(body_text.contains("Visible parent body"));
            assert!(!body_text.contains("Secret child text"));
            assert_eq!(body_html, None);
            assert_eq!(attachment_filenames, ["bundle.mime"]);
        }
        "empty-alternative-leaf" => {
            assert!(body_text.contains("Visible HTML after an empty plain part."));
            assert!(body_html
                .is_some_and(|body| body.contains("Visible HTML after an empty plain part.")));
            assert!(attachment_filenames.is_empty());
        }
        "truncated-multipart-lf" => {
            assert!(
                body_text.contains("second leaf and closing boundary are deliberately missing.")
            );
            assert!(!body_text.contains("--truncated"));
            assert_eq!(body_html, None);
            assert!(attachment_filenames.is_empty());
        }
        "charset-matrix" => {
            assert!(body_text.contains("Zażółć"));
            assert!(!body_text.contains("Unknown charset must not become invented text"));
            assert_eq!(body_html, None);
            assert!(attachment_filenames.is_empty());
        }
        "format-flowed-delsp" => {
            assert!(body_text.contains("soft spacethat continues"));
            assert!(body_text.contains("> Quoted flowed textcontinues too."));
            assert!(body_html.is_some_and(|body| body.contains("This is the HTML alternative.")));
            assert!(attachment_filenames.is_empty());
        }
        "international-reader-semantics" => {
            assert_eq!(subject, "Fictional 🌍 reader message");
            assert!(body_text.contains("Tere, maailma 👋jätkub samal real."));
            assert!(body_text.contains("مرحبا بالعالم"));
            assert!(body_text.contains("שלום עולם"));
            assert!(body_html.is_some_and(|body| body.contains("dir=\"rtl\"")));
            assert!(attachment_filenames.is_empty());
        }
        "ambiguous-inline-identities" => {
            assert!(body_text.contains("Fictional resources"));
            assert!(body_html.is_some_and(|body| body.contains("cid:shared@example.test")));
            assert_eq!(attachment_filenames, ["logo.png", "source.pdf"]);
        }
        other => panic!("unexpected governed MIME Reader fixture: {other}"),
    }
}

fn checked_external_root(nonce: &str) -> Option<PathBuf> {
    let root = std::env::var_os("DAKIA_MIME_READER_CORE_ARTIFACT_ROOT")?;
    let root = PathBuf::from(root);
    assert!(root.is_absolute(), "artifact root must be absolute");
    let root = root
        .canonicalize()
        .expect("artifact root must already exist");
    let marker = std::fs::read_to_string(root.join(ARTIFACT_MARKER))
        .expect("artifact root must have an ownership marker");
    assert_eq!(marker, format!("{nonce}\n"));
    assert!(
        !root.join(PROFILE_NAME).exists(),
        "artifact export refuses to overwrite a profile"
    );
    Some(root)
}

async fn populate_profile(root: &Path, nonce: &str) -> serde_json::Value {
    let mut account = test_account();
    account.id = uuid::Uuid::parse_str("13131313-1313-4313-8313-131313131313")
        .expect("fixed fictional account UUID");
    account.email = "mime-reader-artifact@example.test".into();

    let database = root.join(PROFILE_NAME);
    let store = Store::open(&database).await.expect("open artifact profile");
    store.save_account(&account).await.expect("save account");
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", UID_VALIDITY as u32, 4, true)
        .await
        .expect("save immutable mailbox epoch");

    let mut exported = Vec::new();
    for (offset, fixture_id) in GOVERNED_READER_FIXTURES.into_iter().enumerate() {
        let uid = 701 + offset as u32;
        let mut parsed = parse_message(
            &account,
            "INBOX",
            uid,
            &[],
            governed_fixture(fixture_id),
            None,
        )
        .await
        .expect("governed MIME artifact fixture must parse");
        parsed.mailbox_uid_validity = Some(UID_VALIDITY);
        let parsed_filenames = parsed
            .attachments
            .iter()
            .map(|item| item.attachment.filename.clone())
            .collect::<Vec<_>>();
        assert_reader_semantics(
            fixture_id,
            &parsed.subject,
            &parsed.body_text,
            parsed.body_html.as_deref(),
            &parsed_filenames,
        );
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
                        .map(|item| item.attachment.clone())
                        .collect(),
                },
            )
            .await
            .expect("persist parsed Reader cache");
        exported.push(json!({
            "fixtureId": fixture_id,
            "messageId": parsed.id,
            "expectedUidValidity": UID_VALIDITY,
        }));
    }

    for entry in &exported {
        let message_id = entry["messageId"].as_str().unwrap();
        assert!(store.message(message_id).await.unwrap().is_some());
        let cached = store
            .cached_message_content(message_id)
            .await
            .unwrap()
            .expect("cached Reader content remains available");
        let message = store
            .message(message_id)
            .await
            .unwrap()
            .expect("parsed summary remains available");
        let cached_filenames = cached
            .attachments
            .iter()
            .filter(|attachment| attachment.presentation.is_downloadable())
            .map(|attachment| attachment.filename.clone())
            .collect::<Vec<_>>();
        assert_reader_semantics(
            entry["fixtureId"].as_str().unwrap(),
            &message.subject,
            &cached.body_text,
            cached.body_html.as_deref(),
            &cached_filenames,
        );
    }

    json!({
        "schemaVersion": 1,
        "nonce": nonce,
        "database": PROFILE_NAME,
        "messages": exported,
    })
}

#[tokio::test]
async fn governed_mime_fixtures_export_a_real_reader_profile_for_tauri() {
    let nonce = std::env::var("DAKIA_MIME_READER_ARTIFACT_NONCE")
        .unwrap_or_else(|_| "standalone-core-verification".into());
    let temporary;
    let root = if let Some(root) = checked_external_root(&nonce) {
        root
    } else {
        temporary = tempfile::tempdir().expect("temporary artifact profile");
        temporary.path().to_path_buf()
    };

    let manifest = populate_profile(&root, &nonce).await;
    assert_eq!(
        manifest["messages"].as_array().unwrap().len(),
        GOVERNED_READER_FIXTURES.len()
    );
    if std::env::var_os("DAKIA_MIME_READER_CORE_ARTIFACT_ROOT").is_some() {
        std::fs::write(
            root.join("core-manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .expect("write core artifact manifest");
    }
}
