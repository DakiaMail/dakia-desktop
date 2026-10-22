use super::*;

pub(super) static LARGE_RESOURCE_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

fn opaque_message_at_size(size: usize) -> Vec<u8> {
    let prefix = b"Content-Type: application/octet-stream\r\n\r\n";
    assert!(size >= prefix.len());
    let mut raw = Vec::with_capacity(size);
    raw.extend_from_slice(prefix);
    raw.resize(size, b'x');
    raw
}

fn aggregate_header_bytes(raw: &[u8]) -> usize {
    configured_message_parser()
        .parse(raw)
        .expect("generated aggregate-header message must parse")
        .parts
        .iter()
        .map(|part| {
            part.raw_body_offset()
                .saturating_sub(part.raw_header_offset()) as usize
        })
        .sum()
}

fn aggregate_header_message(first_fill: usize, second_fill: usize) -> Vec<u8> {
    let mut raw = b"Content-Type: multipart/mixed; boundary=h\r\n\r\n".to_vec();
    for (index, fill) in [first_fill, second_fill].into_iter().enumerate() {
        raw.extend_from_slice(b"--h\r\nX-Fill: ");
        raw.extend(std::iter::repeat_n(b'x', fill));
        raw.extend_from_slice(
            format!("\r\nContent-Type: application/octet-stream; name={index}.bin\r\n\r\nx\r\n")
                .as_bytes(),
        );
    }
    raw.extend_from_slice(b"--h--\r\n");
    raw
}

fn message_with_aggregate_header_bytes(target: usize) -> Vec<u8> {
    let fixed = aggregate_header_bytes(&aggregate_header_message(0, 0));
    let fill = target
        .checked_sub(fixed)
        .expect("target aggregate header budget exceeds fixture overhead");
    let raw = aggregate_header_message(fill / 2, fill - (fill / 2));
    assert_eq!(aggregate_header_bytes(&raw), target);
    raw
}

fn attachment_message(count: usize) -> Vec<u8> {
    let mut raw = b"Content-Type: multipart/mixed; boundary=a\r\n\r\n".to_vec();
    for index in 0..count {
        raw.extend_from_slice(
            format!(
                "--a\r\nContent-Type: application/octet-stream; name={index}.bin\r\nContent-Disposition: attachment; filename={index}.bin\r\nContent-Transfer-Encoding: base64\r\n\r\neA==\r\n"
            )
            .as_bytes(),
        );
    }
    raw.extend_from_slice(b"--a--\r\n");
    raw
}

fn multipart_with_leaf_count(count: usize) -> Vec<u8> {
    let mut raw = b"Content-Type: multipart/mixed; boundary=p\r\n\r\n".to_vec();
    for index in 0..count {
        raw.extend_from_slice(
            format!("--p\r\nContent-Type: application/octet-stream; name={index}.bin\r\n\r\nx\r\n")
                .as_bytes(),
        );
    }
    raw.extend_from_slice(b"--p--\r\n");
    raw
}

fn nested_multipart(depth: usize) -> Vec<u8> {
    let mut raw = Vec::new();
    for level in 0..depth {
        raw.extend_from_slice(
            format!("Content-Type: multipart/mixed; boundary=b{level}\r\n\r\n--b{level}\r\n")
                .as_bytes(),
        );
    }
    raw.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n");
    for level in (0..depth).rev() {
        raw.extend_from_slice(format!("--b{level}--\r\n").as_bytes());
    }
    raw
}

#[tokio::test]
async fn production_complete_parser_has_below_exact_and_above_triads_for_every_mime_budget() {
    let _large_buffer_guard = LARGE_RESOURCE_TEST_LOCK.lock().await;

    for (size, expected_error) in [
        (MAX_RAW_MESSAGE_BYTES - 1, None),
        (MAX_RAW_MESSAGE_BYTES, None),
        (
            MAX_RAW_MESSAGE_BYTES + 1,
            Some("mime_raw_message_too_large"),
        ),
    ] {
        let raw = opaque_message_at_size(size);
        match expected_error {
            None => drop(parse_complete_message(&raw).expect("raw boundary must parse")),
            Some(expected) => assert_eq!(
                parse_complete_message(&raw)
                    .expect_err("raw boundary must be rejected")
                    .to_string(),
                expected
            ),
        }
    }

    for (size, expected_error) in [
        (MAX_MIME_HEADER_BYTES - 1, None),
        (MAX_MIME_HEADER_BYTES, None),
        (MAX_MIME_HEADER_BYTES + 1, Some("mime_headers_too_large")),
    ] {
        let raw = message_with_aggregate_header_bytes(size);
        match expected_error {
            None => drop(parse_complete_message(&raw).expect("header boundary must parse")),
            Some(expected) => assert_eq!(
                parse_complete_message(&raw)
                    .expect_err("header boundary must be rejected")
                    .to_string(),
                expected
            ),
        }
    }

    for (leaves, expected_error) in [
        (MAX_MIME_PARTS - 2, None),
        (MAX_MIME_PARTS - 1, None),
        (MAX_MIME_PARTS, Some("mime_too_many_parts")),
    ] {
        let raw = multipart_with_leaf_count(leaves);
        match expected_error {
            None => drop(parse_complete_message(&raw).expect("part boundary must parse")),
            Some(expected) => assert_eq!(
                parse_complete_message(&raw)
                    .expect_err("part boundary must be rejected")
                    .to_string(),
                expected
            ),
        }
    }

    for (depth, expected_error) in [
        (MAX_MULTIPART_NESTING - 1, None),
        (MAX_MULTIPART_NESTING, None),
        (
            MAX_MULTIPART_NESTING + 1,
            Some("mime_multipart_nesting_too_deep"),
        ),
    ] {
        let raw = nested_multipart(depth);
        match expected_error {
            None => drop(parse_complete_message(&raw).expect("depth boundary must parse")),
            Some(expected) => assert_eq!(
                parse_complete_message(&raw)
                    .expect_err("depth boundary must be rejected")
                    .to_string(),
                expected
            ),
        }
    }
}

#[tokio::test]
async fn production_mime_resource_guards_have_below_exact_and_above_triads() {
    let _large_buffer_guard = LARGE_RESOURCE_TEST_LOCK.lock().await;

    assert!(display_literal_limit(0, MAX_DISPLAY_PART_BYTES - 1, "message display part").is_ok());
    assert!(display_literal_limit(0, MAX_DISPLAY_PART_BYTES, "message display part").is_ok());
    assert_eq!(
        display_literal_limit(0, MAX_DISPLAY_PART_BYTES + 1, "message display part")
            .expect_err("above-limit display part must fail")
            .to_string(),
        "message display part exceeds the 25 MiB safety limit"
    );

    for (advertised, expected_error) in [(9, None), (10, None), (11, Some(50))] {
        let result = display_literal_limit(
            MAX_DISPLAY_TOTAL_BYTES - 10,
            advertised,
            "message display part",
        );
        match expected_error {
            None => {
                assert_eq!(result.expect("aggregate display boundary must fit"), 10);
            }
            Some(mebibytes) => assert_eq!(
                result
                    .expect_err("aggregate display boundary must fail")
                    .to_string(),
                format!("message display body exceeds the {mebibytes} MiB safety limit")
            ),
        }
    }

    let mut attachment = vec![0; MAX_ATTACHMENT_BYTES - 1];
    validate_targeted_attachment_bytes(&attachment)
        .expect("below-limit decoded attachment must fit");
    attachment.push(0);
    validate_targeted_attachment_bytes(&attachment)
        .expect("exact-limit decoded attachment must fit");
    attachment.push(0);
    assert!(validate_targeted_attachment_bytes(&attachment).is_err());
    drop(attachment);

    for (part_bytes, expected_error) in [(9, false), (10, false), (11, true)] {
        let mut total = MAX_ATTACHMENT_TOTAL_BYTES - 10;
        let result = add_decoded_attachment_bytes(&mut total, part_bytes);
        if expected_error {
            assert_eq!(
                result
                    .expect_err("above-limit decoded attachment aggregate must fail")
                    .to_string(),
                "message attachments exceed the 50 MiB safety limit"
            );
        } else {
            result.expect("decoded attachment aggregate boundary must fit");
            assert_eq!(total, MAX_ATTACHMENT_TOTAL_BYTES - 10 + part_bytes);
        }
    }

    for (count, expected_error) in [
        (MAX_ATTACHMENT_COUNT - 1, false),
        (MAX_ATTACHMENT_COUNT, false),
        (MAX_ATTACHMENT_COUNT + 1, true),
    ] {
        let raw = attachment_message(count);
        let parsed = parse_complete_message(&raw).expect("attachment-count fixture must parse");
        let result = extract_attachments(&parsed, "resource-budget-message");
        if expected_error {
            assert_eq!(
                result
                    .expect_err("above-limit attachment count must fail")
                    .to_string(),
                format!("message has more than {MAX_ATTACHMENT_COUNT} attachments")
            );
        } else {
            assert_eq!(
                result.expect("attachment count boundary must fit").len(),
                count
            );
        }
    }
}
