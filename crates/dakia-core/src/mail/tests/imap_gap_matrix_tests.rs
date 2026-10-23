//! Explicit IMAP edge-case matrix for the remaining mail-safety catalogue gaps.

use super::*;
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::watch,
    time::{timeout, Duration},
};

#[test]
fn ms_b_010_list_grammar_handles_case_escaping_nil_delimiters_and_unicode() {
    let unicode_logical = "归档/项目";
    let unicode_wire = encode_modified_utf7(unicode_logical).expect("encode legacy mailbox wire");
    let cases = [
        (
            r#"* lIsT (\HasNoChildren \Sent) "." "Sent \"Receipts\"""#.to_owned(),
            vec!["\\hasnochildren", "\\sent"],
            "Sent \"Receipts\"".to_owned(),
        ),
        (
            format!(r#"* LIST (\HasNoChildren) NIL "{unicode_wire}""#),
            vec!["\\hasnochildren"],
            unicode_wire.clone(),
        ),
        (r#"* LIST () "/" NIL"#.to_owned(), vec![], "NIL".to_owned()),
    ];

    for (line, expected_flags, expected_mailbox) in cases {
        let (flags, mailbox) = parse_list_mailbox(&line).expect(&line);
        assert_eq!(flags, expected_flags, "{line}");
        assert_eq!(mailbox, expected_mailbox, "{line}");
    }
    assert_eq!(
        decode_modified_utf7(&unicode_wire).expect("decode canonical mailbox wire"),
        unicode_logical
    );

    assert_eq!(
        parse_imap_astring(r#""escaped\\slash\"quote" tail"#),
        Some(("escaped\\slash\"quote".into(), " tail"))
    );
    let inbox_wire = encode_modified_utf7("收件箱").expect("encode legacy inbox wire");
    assert!(validate_list_response(&[
        format!(r#"* LIST (\HasNoChildren) "/" "{inbox_wire}""#),
        "D0001 OK listed".into(),
    ])
    .is_ok());
    assert!(validate_list_response(&["* LIST () \"/\" {5}".into()]).is_err());
}

#[tokio::test]
async fn ms_b_010_011_fragmented_literals_keep_exact_uid_and_section_identity() {
    let (mut client, server) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server);
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");

        for fragment in [
            b"* 1 fEtCh (UID 7 BODY[1] {5}\r\nwrong)\r\n* 2 FETCH (UID 42 BODY[2] {5}\r\n"
                .as_slice(),
            b"other)\r\n* 3 FETCH (BODY[1] {5}\r\nri".as_slice(),
            b"ght UID 42)\r\n".as_slice(),
        ] {
            write.write_all(fragment).await.unwrap();
            write.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        write
            .write_all(format!("{tag} oK complete\r\n").as_bytes())
            .await
            .unwrap();
    });

    let mut response = client
        .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
        .await
        .unwrap();
    assert_eq!(
        response.take_body_literal_for(42, "1"),
        Some(b"right".to_vec())
    );
    assert!(response.take_body_literal_for(7, "1").is_some());
    assert!(response.take_body_literal_for(42, "2").is_some());
    assert!(response.take_body_literal_for(42, "1").is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn ms_b_011_bad_tags_truncation_lengths_and_oversize_literals_fail_closed() {
    let (mut wrong_tag, wrong_tag_server) = duplex_imap_client_and_server();
    let wrong_tag_server = tokio::spawn(async move {
        let (read, mut write) = split(wrong_tag_server);
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "NOOP");
        write.write_all(b"D9999 OK unrelated\r\n").await.unwrap();
    });
    let error = wrong_tag.command("NOOP").await.unwrap_err();
    assert!(format!("{error:#}").contains("connection closed"));
    wrong_tag_server.await.unwrap();

    let (mut truncated, truncated_server) = duplex_imap_client_and_server();
    let truncated_server = tokio::spawn(async move {
        let (read, mut write) = split(truncated_server);
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
        write
            .write_all(b"* 1 FETCH (UID 42 BODY[1] {5}\r\nabc")
            .await
            .unwrap();
    });
    assert!(truncated
        .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
        .await
        .is_err());
    truncated_server.await.unwrap();

    let (mut malformed, malformed_server) = duplex_imap_client_and_server();
    let malformed_server = tokio::spawn(async move {
        let (read, mut write) = split(malformed_server);
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
        write
            .write_all(
                format!("* 1 FETCH (UID 42 BODY[1] {{NaN}}\r\n{tag} OK complete\r\n").as_bytes(),
            )
            .await
            .unwrap();
    });
    let mut response = malformed
        .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
        .await
        .unwrap();
    assert!(response.take_body_literal_for(42, "1").is_none());
    malformed_server.await.unwrap();

    let (mut oversized, oversized_server) = duplex_imap_client_and_server();
    let oversized_server = tokio::spawn(async move {
        let (read, mut write) = split(oversized_server);
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
        write
            .write_all(b"* 1 FETCH (UID 42 BODY[1] {5}\r\nhello)\r\n")
            .await
            .unwrap();
    });
    let error = match oversized
        .command_with_literal_limited("UID FETCH 42 (BODY.PEEK[1])", 4)
        .await
    {
        Ok(_) => panic!("oversized literal unexpectedly completed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("safety limit"));
    oversized_server.await.unwrap();
}

#[test]
fn ms_b_012_empty_sparse_maximum_uids_and_changing_counts_preserve_identity() {
    assert!(mailbox_snapshot_page_ranges(0).is_empty());
    assert_eq!(mailbox_snapshot_page_ranges(1), vec![(1, 1)]);

    let page = parse_mailbox_snapshot_page(
        &[
            "* 1 FETCH (UID 1 FLAGS ())".into(),
            "* 2 FETCH (UID 4000000000 FLAGS (\\Seen))".into(),
            format!("* 3 FETCH (UID {} FLAGS (\\Flagged))", u32::MAX),
        ],
        3,
    )
    .unwrap();
    assert_eq!(
        page.iter().map(|item| item.uid).collect::<Vec<_>>(),
        vec![1, 4_000_000_000, u32::MAX]
    );
    assert!(!page[0].is_read && !page[0].is_flagged);
    assert!(page[1].is_read && !page[1].is_flagged);
    assert!(!page[2].is_read && page[2].is_flagged);

    let invalid_zero =
        parse_mailbox_snapshot_page(&["* 1 FETCH (UID 0 FLAGS ())".into()], 1).unwrap_err();
    assert!(format!("{invalid_zero:#}").contains("snapshot page was incomplete"));
    assert!(
        parse_mailbox_delta_page(&["* 1 FETCH (UID 0 FLAGS () MODSEQ (2))".into()], 1).is_err()
    );
    assert!(parse_mailbox_snapshot_page(
        &[
            "* 1 FETCH (UID 9 FLAGS ())".into(),
            "* 2 FETCH (UID 9 FLAGS (\\Seen))".into(),
        ],
        2,
    )
    .is_err());

    let initial = MailboxIdentity {
        exists: 3,
        uid_validity: 77,
        uid_next: Some(u32::MAX),
        highest_modseq: Some(100),
    };
    assert!(mailbox_membership_is_stable(&initial, &initial));
    for changed in [
        MailboxIdentity {
            exists: 4,
            ..initial.clone()
        },
        MailboxIdentity {
            uid_next: Some(9),
            ..initial.clone()
        },
        MailboxIdentity {
            uid_validity: 78,
            ..initial.clone()
        },
    ] {
        assert!(!mailbox_membership_is_stable(&initial, &changed));
    }
}

#[tokio::test]
async fn ms_b_014_idle_change_finishes_done_and_missing_idle_uses_poll_mode() {
    let (mut idle_client, idle_server) = duplex_imap_client_and_server();
    let idle_server = tokio::spawn(async move {
        let (read, mut write) = split(idle_server);
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "IDLE");
        write
            .write_all(b"* 9 fEtCh (UID 42 fLaGs (\\Seen))\r\n+ idling\r\n")
            .await
            .unwrap();
        let mut done = String::new();
        read.read_line(&mut done).await.unwrap();
        assert_eq!(done, "DONE\r\n");
        write
            .write_all(format!("{tag} OK idle complete\r\n").as_bytes())
            .await
            .unwrap();
    });
    let (_cancel, mut active) = watch::channel(false);
    assert!(matches!(
        idle_client.idle_until_change(&mut active).await.unwrap(),
        IdleOutcome::Changed
    ));
    idle_server.await.unwrap();

    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let account = test_account();
    store.save_account(&account).await.unwrap();
    let (mut poll_client, poll_server) = duplex_imap_client_and_server();
    let poll_server = tokio::spawn(async move {
        let (read, mut write) = split(poll_server);
        let mut read = BufReader::new(read);
        let identity = "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\n* OK [UIDNEXT 1] next\r\n";
        let mut transcript = Vec::new();
        let tag = scripted_expect_command(&mut read, &mut transcript, "CAPABILITY").await;
        write
            .write_all(format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes())
            .await
            .unwrap();
        for _ in 0..3 {
            let tag = scripted_expect_command(&mut read, &mut transcript, "SELECT INBOX").await;
            write
                .write_all(format!("{identity}{tag} OK selected\r\n").as_bytes())
                .await
                .unwrap();
        }
        assert!(transcript.iter().all(|command| command != "IDLE"));
    });
    let (_cancel, mut active) = watch::channel(false);
    let cycle = service
        .realtime_inbox_cycle_with_client(&mut poll_client, &account, 50, &mut active)
        .await
        .unwrap();
    assert_eq!(cycle.mode, RealtimeMode::Poll);
    assert!(cycle.new_messages.is_empty());
    assert!(
        !cycle.catalogue_changed,
        "an unchanged empty snapshot must not publish a catalogue reload"
    );
    assert!(!cycle.cancelled);
    poll_server.await.unwrap();
}

#[tokio::test]
async fn ms_d_017_complete_public_sync_is_idempotent_after_convergence() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut account = imap_test_account(Security::Tls, port);
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    let store = Store::in_memory().await.unwrap();
    store.save_account(&account).await.unwrap();
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .unwrap();

    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let mut transcripts = Vec::new();
        for pass in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let (read, mut write) = split(stream);
            let mut read = BufReader::new(read);
            write
                .write_all(b"* OK fictional IMAP ready\r\n")
                .await
                .unwrap();
            write.flush().await.unwrap();
            let mut transcript = Vec::new();
            loop {
                let (tag, command) = scripted_command(&mut read).await;
                transcript.push(command.clone());
                let response = if command == "CAPABILITY" {
                    format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n")
                } else if command == "LOGIN \"reader@example.test\" \"sync secret\"" {
                    format!("{tag} OK authenticated\r\n")
                } else if command == "LIST \"\" \"*\"" {
                    format!("* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK listed\r\n")
                } else if command == "SELECT \"INBOX\"" {
                    format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\n* OK [UIDNEXT 43] next\r\n{tag} OK selected\r\n")
                } else if command == "FETCH 1:1 (UID FLAGS)" {
                    format!("* 1 FETCH (UID 42 FLAGS ())\r\n{tag} OK flags\r\n")
                } else if command.starts_with("UID FETCH 42 (FLAGS INTERNALDATE") {
                    assert_eq!(pass, 0, "converged sync fetched message metadata again");
                    let headers = "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Stable fictional message\r\nMessage-ID: <42@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n";
                    format!("* 1 FETCH (UID 42 FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" RFC822.SIZE 5 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1) BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n{headers})\r\n{tag} OK metadata\r\n", headers.len())
                } else if command == "UID FETCH 42 (BODY.PEEK[]<0.8192>)" {
                    assert_eq!(pass, 0, "converged sync fetched the preview again");
                    format!("* 1 FETCH (UID 42 BODY[]<0> {{5}}\r\nhello)\r\n{tag} OK preview\r\n")
                } else if command == "LOGOUT" {
                    write
                        .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                        .await
                        .unwrap();
                    write.flush().await.unwrap();
                    break;
                } else {
                    panic!("unexpected sync command on pass {pass}: {command}");
                };
                write.write_all(response.as_bytes()).await.unwrap();
                write.flush().await.unwrap();
            }
            transcripts.push(transcript);
        }
        transcripts
    });

    let first = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, imap_test_endpoint(port, true))
        .await
        .unwrap();
    assert_eq!(first.synced_count, 1);
    assert!(
        first.new_messages.is_empty(),
        "the first sync establishes the notification baseline"
    );
    let message_before = serde_json::to_value(
        store
            .message_by_locator(account.id, "INBOX", 42)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let state_before = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .unwrap()
        .unwrap();

    let second = service
        .sync_inbox_with_imap_endpoint_for_test(&account, 50, imap_test_endpoint(port, true))
        .await
        .unwrap();
    assert_eq!(second.synced_count, 0);
    assert!(
        second.new_messages.is_empty(),
        "an unchanged sync must not publish an arrival"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .message_by_locator(account.id, "INBOX", 42)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
        message_before
    );
    let state_after = store
        .mailbox_catalog_state(account.id, "INBOX")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            state_after.remote_name,
            state_after.uid_validity,
            state_after.remote_total,
            state_after.historical_complete,
            state_after.uid_next,
            state_after.highest_modseq,
        ),
        (
            state_before.remote_name,
            state_before.uid_validity,
            state_before.remote_total,
            state_before.historical_complete,
            state_before.uid_next,
            state_before.highest_modseq,
        )
    );
    let transcripts = server.await.unwrap();
    assert!(transcripts[1]
        .iter()
        .all(|command| !command.starts_with("UID FETCH")));
}

#[test]
fn ms_b_014_idle_invalidation_matrix_is_specific_and_case_insensitive() {
    for line in [
        "* 1 EXISTS\r\n",
        "* 1 EXPUNGE\r\n",
        "* 9 fEtCh (UID 42 fLaGs (\\Seen))\r\n",
        "* VANISHED 42\r\n",
    ] {
        assert!(is_idle_invalidation(line), "{line:?}");
    }
    for line in [
        "+ idling\r\n",
        "* 9 FETCH (UID 42 BODY[1] {4}\r\n",
        "D0001 OK completed\r\n",
    ] {
        assert!(!is_idle_invalidation(line), "{line:?}");
    }
    assert!(supports_idle(&["* capability imap4rev1 idle".into()]));
    assert!(!supports_idle(&[
        "* CAPABILITY IMAP4rev1 IDLE-NOTIFY".into()
    ]));
}

#[test]
fn ms_b_015_missing_extensions_and_unusual_unicode_folders_have_safe_fallbacks() {
    let capabilities = vec!["* CAPABILITY IMAP4rev1 UIDPLUS".into()];
    assert!(!supports_condstore(&capabilities));
    assert!(!supports_idle(&capabilities));

    let account = test_account();
    let archive_wire = encode_modified_utf7("归档.项目").expect("encode Archive mailbox");
    let sent_wire = encode_modified_utf7("已发送.邮件").expect("encode Sent mailbox");
    let fallback = resolve_special_mailboxes(
        mailbox_plans(&account).unwrap(),
        &[
            r#"* LIST (\HasNoChildren) "." "INBOX""#.into(),
            format!(r#"* LIST (\HasNoChildren) NIL "{archive_wire}""#),
        ],
    );
    assert!(fallback
        .iter()
        .any(|plan| plan.local == "Sent" && plan.remote == "Sent"));
    assert!(fallback
        .iter()
        .any(|plan| plan.local == "Archive" && plan.remote == account.archive_mailbox));

    let discovered = resolve_special_mailboxes(
        mailbox_plans(&account).unwrap(),
        &[
            format!(r#"* LIST (\HasNoChildren \Sent) "." "{sent_wire}""#),
            format!(r#"* LIST (\HasNoChildren \Archive) NIL "{archive_wire}""#),
        ],
    );
    assert!(discovered
        .iter()
        .any(|plan| plan.local == "Sent" && plan.remote == sent_wire));
    assert!(discovered
        .iter()
        .any(|plan| plan.local == "Archive" && plan.remote == archive_wire));
    assert_eq!(
        decode_modified_utf7(&sent_wire).expect("decode discovered Sent mailbox"),
        "已发送.邮件"
    );
    assert_eq!(
        decode_modified_utf7(&archive_wire).expect("decode discovered Archive mailbox"),
        "归档.项目"
    );
}

#[test]
fn modified_utf7_wire_identity_is_case_sensitive_except_for_literal_inbox() {
    let upper = "&ZeVnLIqe-";
    let lower = "&zevnliqe-";
    assert_ne!(
        decode_modified_utf7(upper).unwrap(),
        decode_modified_utf7(lower).unwrap(),
        "case variants are distinct canonical modified UTF-7 mailboxes"
    );
    assert!(!same_mailbox_wire_identity(upper, lower));
    assert!(same_mailbox_wire_identity("INBOX", "inbox"));

    let mut account = test_account();
    account.archive_mailbox = upper.into();
    let resolved = resolve_special_mailboxes(
        mailbox_plans(&account).unwrap(),
        &[
            format!(r#"* LIST (\HasNoChildren \Archive) "/" "{lower}""#),
            format!(r#"* LIST (\HasNoChildren \Archive) "/" "{upper}""#),
        ],
    );
    let archives = resolved
        .iter()
        .filter(|plan| plan.local == "Archive")
        .collect::<Vec<_>>();
    assert_eq!(archives.len(), 2, "distinct wires must not deduplicate");
    assert!(archives.iter().any(|plan| plan.remote == upper));
    assert!(archives.iter().any(|plan| plan.remote == lower));
    assert!(archives
        .iter()
        .any(|plan| plan.remote == upper && plan.storage == "Archive"));
    assert!(archives
        .iter()
        .any(|plan| { plan.remote == lower && plan.storage == format!("Archive::{lower}") }));

    account.provider_id = "gmail".into();
    assert!(!gmail_sender_cleanup_mailbox_is_received(&account, &[], lower).unwrap());
    assert!(gmail_sender_cleanup_mailbox_is_received(&account, &[], upper).unwrap());
}

#[test]
fn move_destination_prefers_listed_configuration_and_rejects_ambiguous_special_use_fallbacks() {
    let mut account = test_account();
    account.archive_mailbox = "日本語".into();
    let configured = encode_modified_utf7(&account.archive_mailbox).unwrap();
    let alternate = encode_modified_utf7("归档").unwrap();
    let another = encode_modified_utf7("存档").unwrap();

    let configured_listing = vec![
        format!(r#"* LIST (\HasNoChildren) "/" "{configured}""#),
        format!(r#"* LIST (\HasNoChildren \Archive) "/" "{alternate}""#),
        "D0001 OK listed".into(),
    ];
    assert_eq!(
        action_destination_from_listing(&account, MailboxAction::Archive, &configured_listing)
            .unwrap(),
        configured,
        "an exact listed account configuration wins over provider fallback flags"
    );

    let ambiguous_listing = vec![
        format!(r#"* LIST (\HasNoChildren \Archive) "/" "{alternate}""#),
        format!(r#"* LIST (\HasNoChildren \Archive) "/" "{another}""#),
        "D0001 OK listed".into(),
    ];
    let error =
        action_destination_from_listing(&account, MailboxAction::Archive, &ambiguous_listing)
            .expect_err("multiple unconfigured Archive targets must fail closed");
    assert!(error.to_string().contains("multiple Archive destinations"));
}

#[test]
fn ms_b_016_gmail_aliases_deduplicate_by_provider_identity_without_cross_account_ids() {
    let mut gmail = test_account();
    gmail.provider_id = "gmail".into();
    let locator = |remote: &str, mailbox: &str, uid| SenderTrashLocalLocator {
        remote: remote.into(),
        mailbox: mailbox.into(),
        uid,
        discovered_uid_validity: 77,
        catalogue_uid_validity: 77,
    };
    let candidates = [
        ("[Gmail]/All Mail", "Archive", 40),
        ("INBOX", "INBOX", 4),
        ("Labels/Customer", "Archive::Labels/Customer", 400),
    ]
    .into_iter()
    .map(|(remote, mailbox, uid)| SenderTrashCandidate {
        remote: remote.into(),
        local_locators: vec![locator(remote, mailbox, uid), locator(remote, mailbox, uid)],
        uid,
        uid_validity: 77,
        gmail_message_id: Some("1000000000000000001".into()),
    })
    .collect();
    let selected = deduplicate_sender_trash_candidates(&gmail, candidates);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].remote, "INBOX");
    assert_eq!(selected[0].local_locators.len(), 3);

    let other_account = uuid::Uuid::new_v4();
    assert_ne!(
        stable_message_id(gmail.id, "INBOX", 4),
        stable_message_id(other_account, "INBOX", 4)
    );
}

#[test]
fn ms_b_017_all_reader_fetch_shapes_are_read_neutral_and_seen_mutation_is_separate() {
    let commands = [
        hydration_fetch_command(42, true),
        hydration_fetch_command(42, false),
        format!("UID FETCH 42 ({})", catalogue_fetch_fields(true)),
        format!("UID FETCH 42 ({})", catalogue_fetch_fields(false)),
        section_fetch_command(42, &[1, 1]),
        section_partial_fetch_command(42, &[1, 1], 8_192),
        raw_message_fetch_command(42),
    ];
    for command in commands {
        assert!(command.contains("BODY.PEEK["), "{command}");
        assert!(
            !command.contains(" RFC822)")
                && !command.contains("(RFC822 ")
                && !command.contains("BODY[]"),
            "{command}"
        );
        assert!(!command.contains("UID STORE"), "{command}");
    }

    assert_eq!(
        set_read_command(42, true),
        "UID STORE 42 +FLAGS.SILENT (\\Seen)"
    );
    assert_eq!(
        set_read_command(42, false),
        "UID STORE 42 -FLAGS.SILENT (\\Seen)"
    );
}

#[tokio::test]
async fn ms_b_011_incomplete_response_does_not_spin() {
    let (mut client, server) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, _) = split(server);
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "NOOP");
    });
    let error = timeout(Duration::from_secs(1), client.command("NOOP"))
        .await
        .expect("closed response must not spin")
        .unwrap_err();
    assert!(format!("{error:#}").contains("connection closed"));
    server.await.unwrap();
}

#[tokio::test]
async fn ms_b_018_logout_bye_requires_the_matching_tagged_ok_and_never_treats_bye_text_as_a_literal(
) {
    let (mut client, server) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server);
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "lOgOuT");
        // `{4}` is ordinary BYE text, not an IMAP literal declaration. The
        // completion arrives in a later write to exercise fragmented framing.
        write
            .write_all(b"* BYE fictional shutdown {4}\r\n")
            .await
            .unwrap();
        write.flush().await.unwrap();
        tokio::task::yield_now().await;
        write
            .write_all(format!("{tag} OK logout complete\r\n").as_bytes())
            .await
            .unwrap();
        write.flush().await.unwrap();
        assert!(
            timeout(Duration::from_millis(50), scripted_command(&mut read))
                .await
                .is_err(),
            "a completed LOGOUT must not be followed by another command"
        );
    });

    let response = client.command("lOgOuT").await.unwrap();
    assert_eq!(
        response,
        [
            "* BYE fictional shutdown {4}\r\n".to_owned(),
            "D0001 OK logout complete\r\n".to_owned(),
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn ms_b_018_logout_bye_rejects_missing_or_wrong_completion_and_bye_remains_fatal_for_noop() {
    for (label, command, response) in [
        ("logout-eof", "LOGOUT", "* BYE closing\r\n"),
        (
            "logout-wrong-tag",
            "LOGOUT",
            "* BYE closing\r\nD9999 OK unrelated\r\n",
        ),
        (
            "noop-bye",
            "NOOP",
            "* BYE closing\r\nD0001 OK unrelated\r\n",
        ),
    ] {
        let (mut client, server) = duplex_imap_client_and_server();
        let server = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut read = BufReader::new(read);
            let (_tag, observed) = scripted_command(&mut read).await;
            assert_eq!(observed, command, "{label}");
            write.write_all(response.as_bytes()).await.unwrap();
            write.flush().await.unwrap();
            assert!(
                timeout(Duration::from_millis(50), scripted_command(&mut read))
                    .await
                    .is_err(),
                "{label}: a failed command must not issue a follow-up command"
            );
        });

        let error = client.command(command).await.unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("closed the connection") || rendered.contains("connection closed"),
            "{label}: {rendered}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn ms_b_018_logout_tagged_ok_without_bye_remains_compatible() {
    let (mut client, server) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server);
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "LOGOUT");
        write
            .write_all(format!("{tag} OK logout complete\r\n").as_bytes())
            .await
            .unwrap();
        write.flush().await.unwrap();
        assert!(
            timeout(Duration::from_millis(50), scripted_command(&mut read))
                .await
                .is_err(),
            "a completed LOGOUT must not be followed by another command"
        );
    });

    assert_eq!(
        client.command("LOGOUT").await.unwrap(),
        ["D0001 OK logout complete\r\n".to_owned()]
    );
    server.await.unwrap();
}
