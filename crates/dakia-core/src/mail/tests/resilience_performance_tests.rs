use super::*;
use crate::storage::MessageContentFetchAcquire;
use std::{path::Path, process::Command};
use tokio::time::{timeout, Duration, Instant};

async fn measured_mailbox_snapshot(message_count: u32) {
    let started = Instant::now();
    assert_large_message_snapshot_is_bounded(message_count).await;
    let elapsed = started.elapsed();
    println!(
        "DAKIA_MAILBOX_PERF {{\"messageCount\":{message_count},\"pageSize\":{MAILBOX_SNAPSHOT_PAGE_SIZE},\"pageCount\":{},\"elapsedMilliseconds\":{}}}",
        message_count.div_ceil(MAILBOX_SNAPSHOT_PAGE_SIZE),
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn scripted_two_thousand_message_snapshot_uses_bounded_pages() {
    assert_large_message_snapshot_is_bounded(2_000).await;
}

#[tokio::test]
async fn cancelled_large_snapshot_preserves_committed_catalogue_and_drops_transport() {
    for message_count in [2_000_u32, 30_000, 100_000, 200_000] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join(format!("cancelled-{message_count}.sqlite"));
        let store = Store::open(&database).await.unwrap();
        let service = MailService::new(store.clone());
        let account = test_account();
        store.save_account(&account).await.unwrap();
        service
            .credentials()
            .set_password(&account, "sync secret")
            .await
            .unwrap();
        store
            .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 1, true)
            .await
            .unwrap();
        let committed = snapshot_test_message(&account, 1);
        store
            .save_synced_messages_through(
                account.id,
                "INBOX",
                std::slice::from_ref(&committed),
                Some(1),
            )
            .await
            .unwrap();

        let ranges = mailbox_snapshot_page_ranges(message_count);
        assert_eq!(
            ranges.len(),
            message_count.div_ceil(MAILBOX_SNAPSHOT_PAGE_SIZE) as usize
        );
        let first_page = ranges[0];
        let second_page = ranges[1];
        assert_eq!(first_page.1 - first_page.0 + 1, MAILBOX_SNAPSHOT_PAGE_SIZE);
        assert_eq!(
            second_page.1 - second_page.0 + 1,
            MAILBOX_SNAPSHOT_PAGE_SIZE
        );

        let (mut client, server) = duplex_imap_client_and_server();
        let (second_page_reached, wait_for_second_page) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut read = BufReader::new(read);
            let mut transcript = Vec::new();

            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                "LOGIN \"reader@example.test\" \"sync secret\"",
            )
            .await;
            write
                .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                .await
                .unwrap();
            let tag = scripted_expect_command(&mut read, &mut transcript, "CAPABILITY").await;
            write
                .write_all(format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes())
                .await
                .unwrap();
            let tag = scripted_expect_command(&mut read, &mut transcript, "LIST \"\" \"*\"").await;
            write
                .write_all(
                    format!("* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK listed\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let tag = scripted_expect_command(&mut read, &mut transcript, "SELECT \"INBOX\"").await;
            write
                .write_all(
                    format!(
                        "* {message_count} EXISTS\r\n* OK [UIDVALIDITY 77] UIDs valid\r\n* OK [UIDNEXT {}] Predicted next UID\r\n{tag} OK selected\r\n",
                        message_count + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();

            let first_command = format!("FETCH {}:{} (UID FLAGS)", first_page.0, first_page.1);
            let tag = scripted_expect_command(&mut read, &mut transcript, &first_command).await;
            for sequence in first_page.0..=first_page.1 {
                write
                    .write_all(
                        format!("* {sequence} FETCH (UID {sequence} FLAGS ())\r\n").as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            write
                .write_all(format!("{tag} OK first page complete\r\n").as_bytes())
                .await
                .unwrap();

            let second_command = format!("FETCH {}:{} (UID FLAGS)", second_page.0, second_page.1);
            scripted_expect_command(&mut read, &mut transcript, &second_command).await;
            second_page_reached.send(()).unwrap();
            let mut byte = [0_u8; 1];
            let read_count = timeout(Duration::from_secs(1), read.read(&mut byte))
                .await
                .expect("cancelled snapshot must release its transport promptly")
                .unwrap();
            assert_eq!(read_count, 0, "cancelled snapshot transport must reach EOF");
        });

        let synced_account = account.clone();
        let sync = tokio::spawn(async move {
            service
                .sync_mailboxes_with_progress_on_client(
                    &mut client,
                    &synced_account,
                    50,
                    vec![MailboxPlan::new("INBOX", "INBOX")],
                    false,
                    |_| {},
                )
                .await
        });
        wait_for_second_page.await.unwrap();
        sync.abort();
        assert!(sync.await.unwrap_err().is_cancelled());
        server.await.unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();
        assert_eq!(
            reopened.mailbox_uids(account.id, "INBOX").await.unwrap(),
            [1].into(),
            "an interrupted {message_count}-message inventory must not delete the committed catalogue"
        );
        let catalogue = reopened
            .mailbox_catalog_state(account.id, "INBOX")
            .await
            .unwrap()
            .expect("committed catalogue identity must survive interruption");
        assert_eq!(catalogue.uid_validity, 77);
        assert_eq!(catalogue.remote_total, 1);
        assert!(catalogue.historical_complete);
        let state = reopened
            .prepare_mailbox_sync(account.id, "INBOX", Some(77))
            .await
            .unwrap();
        assert_eq!(state.highest_uid, Some(1));
        assert!(state.initialized);
        assert!(!state.uid_validity_changed);
        assert_eq!(
            reopened
                .message(&committed.id)
                .await
                .unwrap()
                .unwrap()
                .subject,
            committed.subject
        );
    }
}

#[tokio::test]
async fn cancelling_a_content_fetch_owner_releases_its_database_claim() {
    let store = Store::in_memory().await.unwrap();
    let account = test_account();
    store.save_account(&account).await.unwrap();
    let message = snapshot_test_message(&account, 1);
    let message_id = message.id.clone();
    store.upsert_catalog_messages(&[message]).await.unwrap();

    let claim_store = store.clone();
    let claim_id = message_id.clone();
    let (claimed, wait_until_claimed) = oneshot::channel();
    let owner = tokio::spawn(async move {
        let claim = match claim_store
            .acquire_message_content_fetch_outcome(&claim_id)
            .await
            .unwrap()
        {
            MessageContentFetchAcquire::Claimed(claim) => claim,
            MessageContentFetchAcquire::Busy | MessageContentFetchAcquire::Missing => {
                panic!("fixture claim must be available")
            }
        };
        claimed.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(claim);
    });
    wait_until_claimed.await.unwrap();
    assert!(matches!(
        store
            .acquire_message_content_fetch_outcome(&message_id)
            .await
            .unwrap(),
        MessageContentFetchAcquire::Busy
    ));

    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    let reacquired = timeout(Duration::from_secs(1), async {
        loop {
            if let MessageContentFetchAcquire::Claimed(claim) = store
                .acquire_message_content_fetch_outcome(&message_id)
                .await
                .unwrap()
            {
                break claim;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled owner must release the durable fetch claim");
    reacquired.release().await.unwrap();
}

fn open_file_descriptor_count() -> Option<usize> {
    [Path::new("/dev/fd"), Path::new("/proc/self/fd")]
        .into_iter()
        .find_map(|path| std::fs::read_dir(path).ok().map(Iterator::count))
}

fn resident_set_bytes() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

async fn one_reconnect_cycle() {
    let (mut client, server) = plain_imap_client_and_server().await;
    let server = tokio::spawn(async move {
        let (read, mut write) = server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "NOOP");
        write
            .write_all(format!("{tag} OK active\r\n").as_bytes())
            .await
            .unwrap();
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "LOGOUT");
        write
            .write_all(format!("{tag} OK logout\r\n").as_bytes())
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        let read_count = timeout(Duration::from_secs(1), read.read(&mut byte))
            .await
            .expect("logged-out client must release its socket")
            .unwrap();
        assert_eq!(read_count, 0);
    });
    client.command("NOOP").await.unwrap();
    client.command("LOGOUT").await.unwrap();
    drop(client);
    server.await.unwrap();
}

#[tokio::test]
#[ignore = "manual host resource soak lane"]
async fn manual_repeated_reconnect_and_store_open_close_soak() {
    const RECONNECT_CYCLES: usize = 200;
    const STORE_OPEN_CYCLES: usize = 100;
    const MAX_RESIDENT_GROWTH_BYTES: u64 = 64 * 1024 * 1024;

    for _ in 0..5 {
        one_reconnect_cycle().await;
    }
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("reconnect-soak.sqlite3");
    let account = test_account();
    let initial = Store::open(&database).await.unwrap();
    initial.save_account(&account).await.unwrap();
    drop(initial);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let descriptors_before = open_file_descriptor_count()
        .expect("manual resource soak requires /dev/fd or /proc/self/fd");
    let resident_before = resident_set_bytes().expect("manual resource soak requires ps RSS");
    let started = Instant::now();
    for _ in 0..RECONNECT_CYCLES {
        one_reconnect_cycle().await;
    }
    for _ in 0..STORE_OPEN_CYCLES {
        let reopened = Store::open(&database).await.unwrap();
        assert!(reopened.account(account.id).await.unwrap().is_some());
        drop(reopened);
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let elapsed = started.elapsed();
    let descriptors_after = open_file_descriptor_count().unwrap();
    let resident_after = resident_set_bytes().unwrap();
    let resident_growth = resident_after.saturating_sub(resident_before);

    assert!(
        descriptors_after <= descriptors_before + 3,
        "resource soak leaked file descriptors: before={descriptors_before}, after={descriptors_after}"
    );
    assert!(
        resident_growth <= MAX_RESIDENT_GROWTH_BYTES,
        "resource soak grew resident memory by {resident_growth} bytes"
    );
    println!(
        "DAKIA_RESOURCE_SOAK {{\"reconnectCycles\":{RECONNECT_CYCLES},\"storeOpenCycles\":{STORE_OPEN_CYCLES},\"elapsedMilliseconds\":{},\"fileDescriptorsBefore\":{descriptors_before},\"fileDescriptorsAfter\":{descriptors_after},\"residentBytesBefore\":{resident_before},\"residentBytesAfter\":{resident_after},\"residentGrowthBytes\":{resident_growth},\"residentGrowthLimitBytes\":{MAX_RESIDENT_GROWTH_BYTES}}}",
        elapsed.as_millis()
    );
}

// These cases deliberately stay out of the ordinary PR lane. The checked-in
// runner invokes them serially and records the host plus repeat measurements.
#[tokio::test]
#[ignore = "manual fixed-hardware performance lane"]
async fn manual_two_thousand_message_snapshot_baseline() {
    measured_mailbox_snapshot(2_000).await;
}

#[tokio::test]
#[ignore = "manual fixed-hardware performance lane"]
async fn manual_thirty_thousand_message_snapshot_baseline() {
    measured_mailbox_snapshot(30_000).await;
}

#[tokio::test]
#[ignore = "manual fixed-hardware performance lane"]
async fn manual_one_hundred_thousand_message_snapshot_baseline() {
    measured_mailbox_snapshot(100_000).await;
}

#[tokio::test]
#[ignore = "manual fixed-hardware performance lane"]
async fn manual_two_hundred_thousand_message_snapshot_baseline() {
    measured_mailbox_snapshot(200_000).await;
}

#[tokio::test]
async fn cancelled_idle_closes_socket_and_a_new_connection_progresses() {
    let (mut first_client, first_server) = plain_imap_client_and_server().await;
    let first_server = tokio::spawn(async move {
        let (read, mut write) = first_server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "IDLE");
        write.write_all(b"+ idling\r\n").await.unwrap();
        let mut done = String::new();
        read.read_line(&mut done).await.unwrap();
        assert_eq!(done, "DONE\r\n");
        write
            .write_all(format!("{tag} OK IDLE completed\r\n").as_bytes())
            .await
            .unwrap();

        let mut byte = [0_u8; 1];
        let read_count = timeout(Duration::from_secs(1), read.read(&mut byte))
            .await
            .expect("cancelled client must release its socket promptly")
            .unwrap();
        assert_eq!(read_count, 0, "the cancelled client socket must reach EOF");
    });

    let (cancel, mut cancelled) = watch::channel(false);
    let first_cycle = tokio::spawn(async move {
        let result = first_client.idle_until_change(&mut cancelled).await;
        drop(first_client);
        result
    });
    tokio::task::yield_now().await;
    cancel.send(true).unwrap();
    assert!(matches!(
        first_cycle.await.unwrap().unwrap(),
        IdleOutcome::Cancelled
    ));
    first_server.await.unwrap();

    let (mut reconnect, reconnect_server) = plain_imap_client_and_server().await;
    let reconnect_server = tokio::spawn(async move {
        let (read, mut write) = reconnect_server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "NOOP");
        write
            .write_all(format!("{tag} OK reconnected\r\n").as_bytes())
            .await
            .unwrap();
    });
    reconnect.command("NOOP").await.unwrap();
    reconnect_server.await.unwrap();
}
