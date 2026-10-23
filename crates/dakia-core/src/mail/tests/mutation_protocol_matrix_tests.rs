//! Exact IMAP command and receipt coverage for the complete move-action inventory.

use super::*;
use tokio::io::{split, AsyncWriteExt, BufReader};

#[tokio::test]
async fn complete_move_action_inventory_preserves_source_and_destination_identity() {
    let account = test_account();
    let cases = [
        (MailboxAction::Archive, "INBOX", "Archive", 201_u32),
        (MailboxAction::Spam, "INBOX", "Spam", 202_u32),
        (MailboxAction::NotSpam, "Spam", "INBOX", 203_u32),
        (MailboxAction::Trash, "INBOX", "Trash", 204_u32),
    ];

    for (action, source, destination, destination_uid) in cases {
        let (mut client, server) = duplex_imap_client_and_server();
        let source = source.to_owned();
        let destination = destination.to_owned();
        let server_source = source.clone();
        let server_destination = destination.clone();
        let server = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut read = BufReader::new(read);
            let mut transcript = Vec::new();

            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                "LOGIN \"reader@example.test\" \"move secret\"",
            )
            .await;
            write
                .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                .await
                .unwrap();

            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                &format!("SELECT {}", quote_imap(&server_source)),
            )
            .await;
            write
                .write_all(
                    format!("* OK [UIDVALIDITY 77] source epoch\r\n{tag} OK selected\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();

            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                &format!("UID MOVE 42 {}", quote_imap(&server_destination)),
            )
            .await;
            write
                .write_all(
                    format!("{tag} OK [COPYUID 88 42 {destination_uid}] move completed\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();

            let tag = scripted_expect_command(&mut read, &mut transcript, "LOGOUT").await;
            write
                .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                .await
                .unwrap();
            transcript
        });

        let receipt = MailService::apply_action_with_client_receipt(
            &mut client,
            &account,
            "move secret",
            &source,
            42,
            action,
            Some(77),
            Some(&destination),
        )
        .await
        .unwrap();
        assert_eq!(
            receipt,
            MailboxMoveReceipt {
                destination: Some(MailboxMoveDestination {
                    remote_mailbox: destination.clone(),
                    uid_validity: 88,
                    uid: destination_uid,
                }),
            }
        );
        assert_eq!(
            server.await.unwrap(),
            vec![
                "LOGIN \"reader@example.test\" \"move secret\"".to_owned(),
                format!("SELECT {}", quote_imap(&source)),
                format!("UID MOVE 42 {}", quote_imap(&destination)),
                "LOGOUT".to_owned(),
            ]
        );
    }
}
