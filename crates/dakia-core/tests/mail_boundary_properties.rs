use dakia_core::mail::{is_potentially_unsafe, safe_attachment_filename, safe_mime_type};
use mail_parser::{MessageParser, MimeHeaders};

const BANNED_FILENAME_CHARACTERS: &[char] = &[
    ':', '<', '>', '"', '|', '?', '*', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}',
    '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
];

#[test]
fn fixed_seed_filenames_are_bounded_idempotent_and_path_safe() {
    for seed in [1, 0x51f15e, 0xc0ffee, u32::MAX] {
        let mut random = XorShift32(seed);
        for index in 0..512 {
            let candidate = random_filename(&mut random);
            let sanitized = safe_attachment_filename(&candidate, index);

            assert!(!sanitized.is_empty(), "seed={seed} input={candidate:?}");
            assert!(
                sanitized.len() <= 180,
                "seed={seed} output exceeded byte limit: {sanitized:?}"
            );
            assert!(
                !sanitized.chars().any(char::is_control),
                "seed={seed} output retained a control: {sanitized:?}"
            );
            assert!(
                !sanitized
                    .chars()
                    .any(|character| BANNED_FILENAME_CHARACTERS.contains(&character)),
                "seed={seed} output retained a banned character: {sanitized:?}"
            );
            assert!(!sanitized.contains(['/', '\\']));
            assert_eq!(
                safe_attachment_filename(&sanitized, index),
                sanitized,
                "sanitization must be idempotent for seed={seed}"
            );
        }
    }
}

#[test]
fn long_multibyte_filenames_preserve_utf8_boundaries_and_short_extensions() {
    for seed in [7, 73, 7331, 0xdeadbeef] {
        let mut random = XorShift32(seed);
        for index in 0..128 {
            let stem = (0..256)
                .map(|_| ["é", "猫", "🙂", "a"][random.next_usize(4)])
                .collect::<String>();
            let extension = ["pdf", "tar.gz", "eml", "txt"][random.next_usize(4)];
            let candidate = format!("{stem}.{extension}");
            let sanitized = safe_attachment_filename(&candidate, index);
            let final_extension = extension.rsplit('.').next().unwrap();

            assert!(sanitized.len() <= 180);
            assert!(sanitized.ends_with(&format!(".{final_extension}")));
            assert!(std::str::from_utf8(sanitized.as_bytes()).is_ok());
        }
    }
}

#[test]
fn fixed_seed_mime_types_are_canonical_or_fail_closed() {
    let valid = [
        ("TEXT/PLAIN", "text/plain"),
        ("application/vnd.api+json", "application/vnd.api+json"),
        (" image/svg+xml ", "image/svg+xml"),
        ("application/x-custom_1", "application/x-custom_1"),
    ];
    for (input, expected) in valid {
        assert_eq!(safe_mime_type(input), expected);
        assert_eq!(safe_mime_type(&safe_mime_type(input)), expected);
    }

    for invalid in [
        "",
        "text",
        "/plain",
        "text/",
        "text/plain; charset=utf-8",
        "text/plain\r\nX-Evil: yes",
        "text//plain",
        "te xt/plain",
        "🦀/plain",
    ] {
        assert_eq!(safe_mime_type(invalid), "application/octet-stream");
    }
}

#[test]
fn executable_detection_is_case_insensitive_and_survives_mime_normalization() {
    for extension in [
        "app", "bat", "cmd", "com", "command", "exe", "js", "jse", "msi", "ps1", "scpt", "sh",
        "vbs", "wsf",
    ] {
        assert!(is_potentially_unsafe(
            &format!("invoice.{extension}"),
            "application/octet-stream"
        ));
        assert!(is_potentially_unsafe(
            &format!("INVOICE.{}", extension.to_ascii_uppercase()),
            "application/octet-stream"
        ));
    }

    for mime in [
        "APPLICATION/X-MSDOWNLOAD",
        "APPLICATION/X-SH",
        "APPLICATION/X-APPLE-DISKIMAGE",
    ] {
        assert!(is_potentially_unsafe(
            "innocent-name",
            &safe_mime_type(mime)
        ));
    }

    assert!(is_potentially_unsafe(
        "invoice.pdf.exe",
        "application/octet-stream"
    ));
    assert!(!is_potentially_unsafe("invoice.exe.pdf", "application/pdf"));
}

#[test]
fn pinned_mail_parser_runtime_decodes_filename_parameters_and_content_ids_as_expected() {
    let filenames = MessageParser::new()
        .parse(include_bytes!(
            "../testdata/mime/filename-parameters-and-encodings.eml"
        ))
        .expect("checked-in filename fixture must parse");
    let decoded_names = filenames
        .parts
        .iter()
        .filter_map(|part| part.attachment_name())
        .map(|name| safe_attachment_filename(name, 0))
        .collect::<Vec<_>>();

    assert!(decoded_names.contains(&"quarterly report final.pdf".to_owned()));
    assert!(decoded_names.contains(&"report-test.txt".to_owned()));
    assert!(decoded_names.contains(&"invoice;final.pdf".to_owned()));
    assert!(decoded_names
        .iter()
        .all(|name| !name.contains(['/', '\\', '\u{202e}'])));

    let cid_message = MessageParser::new()
        .parse(include_bytes!(
            "../testdata/mime/linkedin-inline-content-id.eml"
        ))
        .expect("checked-in CID fixture must parse");
    let content_ids = cid_message
        .parts
        .iter()
        .filter_map(|part| part.content_id())
        .collect::<Vec<_>>();

    assert_eq!(content_ids, ["text-body", "html-body"]);
    assert_eq!(
        cid_message.body_text(0).as_deref().map(str::trim),
        Some("Redacted plain text=content with a foldedline.")
    );
    assert!(cid_message
        .body_html(0)
        .as_deref()
        .is_some_and(|html| html.contains("Redacted HTML=content")));
}

struct XorShift32(u32);

impl XorShift32 {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        self.next() as usize % upper_bound
    }
}

fn random_filename(random: &mut XorShift32) -> String {
    const ALPHABET: &[&str] = &[
        "a", "Z", "0", " ", ".", "/", "\\", ":", "<", ">", "\"", "|", "?", "*", "\0", "\r", "\n",
        "\u{200f}", "\u{202e}", "\u{2066}", "é", "猫", "🙂",
    ];
    let length = random.next_usize(260);
    (0..length)
        .map(|_| ALPHABET[random.next_usize(ALPHABET.len())])
        .collect()
}
