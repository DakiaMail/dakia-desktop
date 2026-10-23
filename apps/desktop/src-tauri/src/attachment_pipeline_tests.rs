#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[test]
fn native_attachment_save_preserves_opaque_binary_bytes() {
    let directory = tempdir().expect("temporary Downloads directory");
    let raw = b"PDF\0\xff\x80\r\nopaque";
    let path = super::save_private_download(directory.path(), "invoice.pdf", raw)
        .expect("save binary attachment");

    assert_eq!(std::fs::read(&path).expect("read saved attachment"), raw);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&path)
            .expect("saved attachment metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "mail attachments must never become executable when saved"
    );
}

#[test]
fn native_attachment_save_keeps_near_name_max_and_unicode_eml_export_bytes_readable() {
    let directory = tempdir().expect("temporary Downloads directory");
    let near_name_max = "x".repeat(248);
    let near_name_max_bytes = b"near NAME_MAX attachment";
    let saved = super::save_private_download(directory.path(), &near_name_max, near_name_max_bytes)
        .expect("near NAME_MAX attachment filename must save");
    assert_eq!(
        saved.file_name().and_then(|name| name.to_str()),
        Some(near_name_max.as_str())
    );
    assert_eq!(std::fs::read(&saved).unwrap(), near_name_max_bytes);

    let export_name = super::eml_export_filename(&"€".repeat(180));
    let export_bytes = b"unicode subject message export";
    let exported = super::save_private_download(directory.path(), &export_name, export_bytes)
        .expect("Unicode export filename must save");
    assert_eq!(
        exported.file_name().and_then(|name| name.to_str()),
        Some(export_name.as_str())
    );
    assert_eq!(std::fs::read(exported).unwrap(), export_bytes);
}

#[test]
fn native_attachment_save_matrix_keeps_parser_normalized_names_and_warning_classification() {
    let directory = tempdir().expect("temporary Downloads directory");
    let cases = [
        (
            "quarterly report final.pdf",
            "application/pdf",
            "quarterly report final.pdf",
            false,
        ),
        (
            "../../invoice;final.pdf",
            "application/octet-stream",
            "invoice;final.pdf",
            false,
        ),
        (
            "invoice\u{202e}gpj.exe",
            "application/pdf",
            "invoicegpj.exe",
            true,
        ),
        (
            "review.pdf.command",
            "application/pdf",
            "review.pdf.command",
            true,
        ),
        (
            "/private/receipt.pdf",
            "application/x-msdownload",
            "receipt.pdf",
            true,
        ),
    ];

    for (index, (supplied, mime_type, expected, unsafe_attachment)) in cases.iter().enumerate() {
        let filename = dakia_core::mail::safe_attachment_filename(supplied, index);
        assert_eq!(filename, *expected, "parser-normalized filename");
        assert_eq!(
            dakia_core::mail::is_potentially_unsafe(&filename, mime_type),
            *unsafe_attachment,
            "warning classification for {supplied:?}"
        );

        let bytes = format!("fictional attachment {index}").into_bytes();
        let saved = super::save_private_download(directory.path(), &filename, &bytes)
            .expect("save parser-normalized attachment");
        assert_eq!(
            std::fs::read(&saved).expect("read saved attachment"),
            bytes,
            "the native save boundary must keep opaque bytes for {filename:?}"
        );
        assert!(saved.starts_with(directory.path()));
    }
}

#[test]
fn native_attachment_save_rejects_unsafe_path_components_before_writing() {
    let directory = tempdir().expect("temporary Downloads directory");
    for filename in [
        "../invoice.command",
        "/tmp/invoice.command",
        "nested/invoice.command",
        "",
    ] {
        assert!(
            super::save_private_download(directory.path(), filename, b"fictional bytes").is_err(),
            "{filename:?} must not become a Downloads path"
        );
    }
    assert!(
        std::fs::read_dir(directory.path())
            .expect("read Downloads directory")
            .next()
            .is_none(),
        "a rejected filename must not leave an empty or partial file"
    );
}

#[cfg(unix)]
#[test]
fn native_attachment_save_allows_a_configured_directory_symlink_but_never_follows_a_leaf_symlink() {
    use std::os::unix::fs::symlink;

    let configuration = tempdir().expect("temporary configured Downloads parent");
    let actual_downloads = tempdir().expect("temporary Downloads target");
    let configured_downloads = configuration.path().join("Downloads");
    symlink(actual_downloads.path(), &configured_downloads)
        .expect("user-configured Downloads directory symlink");

    let first = super::save_private_download(&configured_downloads, "normal.pdf", b"normal")
        .expect("a configured Downloads directory symlink remains usable");
    assert_eq!(
        std::fs::read(actual_downloads.path().join("normal.pdf")).unwrap(),
        b"normal"
    );
    assert!(first.starts_with(&configured_downloads));

    let outside = tempdir().expect("outside file parent");
    let outside_file = outside.path().join("do-not-overwrite.pdf");
    std::fs::write(&outside_file, b"outside original").expect("outside fixture");
    symlink(&outside_file, actual_downloads.path().join("invoice.pdf"))
        .expect("malicious leaf symlink");

    let saved = super::save_private_download(&configured_downloads, "invoice.pdf", b"mail bytes")
        .expect("save must choose a non-symlink collision name");
    assert_eq!(
        saved.file_name().and_then(|name| name.to_str()),
        Some("invoice (1).pdf")
    );
    assert_eq!(std::fs::read(&outside_file).unwrap(), b"outside original");
    assert_eq!(
        std::fs::read(actual_downloads.path().join("invoice (1).pdf")).unwrap(),
        b"mail bytes"
    );
}

#[cfg(unix)]
#[test]
fn native_attachment_save_skips_existing_collision_names_before_durable_temporary_work() {
    use std::{io::Write, os::unix::fs::symlink};

    let directory = tempdir().expect("temporary Downloads directory");
    let outside = tempdir().expect("outside symlink target directory");
    let protected = outside.path().join("protected.pdf");
    std::fs::write(&protected, b"must remain unchanged").expect("protected target");

    for counter in 0..9_999 {
        let candidate = directory
            .path()
            .join(super::download_name("report.pdf", counter));
        if counter == 4_000 {
            symlink(&protected, candidate).expect("existing leaf symlink collision");
        } else {
            std::fs::write(candidate, b"existing collision").expect("existing collision");
        }
    }

    let mut writes = 0;
    let mut quarantines = 0;
    let mut temporary_opens = 0;
    let mut write = |file: &mut std::fs::File, bytes: &[u8]| {
        writes += 1;
        file.write_all(bytes)
    };
    let mut quarantine = |_: &std::fs::File| {
        quarantines += 1;
        Ok(())
    };
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || {
        temporary_opens += 1;
        Ok(())
    };
    let mut before_publish = || Ok(());
    let mut after_publish = || Ok(());

    let saved = super::save_private_download_with_directory_descriptor_and_cleanup(
        directory.path(),
        "report.pdf",
        b"new attachment",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        super::private_download_candidate_exists_at,
        super::unlink_private_download_at,
        |_| Ok(()),
    )
    .expect("the final unused collision name must save");

    assert_eq!(
        saved.file_name().and_then(|name| name.to_str()),
        Some("report (9999).pdf")
    );
    assert_eq!(writes, 1, "occupied names must not be written and fsynced");
    assert_eq!(
        quarantines, 1,
        "occupied names must not receive unnecessary quarantine work"
    );
    assert_eq!(
        temporary_opens, 1,
        "occupied names must not allocate private temporary files"
    );
    assert_eq!(std::fs::read(protected).unwrap(), b"must remain unchanged");
}

#[cfg(unix)]
#[test]
fn native_attachment_save_rechecks_exclusivity_when_a_leaf_appears_after_the_probe() {
    use std::{io::Write, os::unix::fs::symlink};

    let directory = tempdir().expect("temporary Downloads directory");
    let outside = tempdir().expect("outside symlink target directory");
    let protected = outside.path().join("protected.command");
    std::fs::write(&protected, b"must remain unchanged").expect("protected target");
    let raced_name = directory.path().join("invoice.command");
    let mut injected = false;
    let mut write = |file: &mut std::fs::File, bytes: &[u8]| file.write_all(bytes);
    let mut quarantine = |_: &std::fs::File| Ok(());
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || Ok(());
    let mut before_publish = || {
        if !injected {
            symlink(&protected, &raced_name).expect("inject leaf after absence probe");
            injected = true;
        }
        Ok(())
    };
    let mut after_publish = || Ok(());

    let saved = super::save_private_download_with_directory_descriptor_and_cleanup(
        directory.path(),
        "invoice.command",
        b"new attachment",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        super::private_download_candidate_exists_at,
        super::unlink_private_download_at,
        |file| file.sync_all(),
    )
    .expect("exclusive publish must retry with a new name after a race");

    assert_eq!(
        saved.file_name().and_then(|name| name.to_str()),
        Some("invoice (1).command")
    );
    assert_eq!(std::fs::read(&protected).unwrap(), b"must remain unchanged");
    assert_eq!(std::fs::read(saved).unwrap(), b"new attachment");
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dakia-")),
        "the raced temporary file must be cleaned through the pinned descriptor"
    );
}

#[cfg(unix)]
#[test]
fn native_attachment_save_exhausts_existing_names_without_temporary_work() {
    let directory = tempdir().expect("temporary Downloads directory");
    for counter in 0..10_000 {
        std::fs::write(
            directory
                .path()
                .join(super::download_name("report.pdf", counter)),
            b"existing collision",
        )
        .expect("existing collision");
    }

    let mut writes = 0;
    let mut quarantines = 0;
    let mut temporary_opens = 0;
    let mut write = |_: &mut std::fs::File, _: &[u8]| {
        writes += 1;
        Ok(())
    };
    let mut quarantine = |_: &std::fs::File| {
        quarantines += 1;
        Ok(())
    };
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || {
        temporary_opens += 1;
        Ok(())
    };
    let mut before_publish = || Ok(());
    let mut after_publish = || Ok(());

    let error = super::save_private_download_with_directory_descriptor_and_cleanup(
        directory.path(),
        "report.pdf",
        b"must not be written",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        super::private_download_candidate_exists_at,
        super::unlink_private_download_at,
        |file| file.sync_all(),
    )
    .expect_err("all reserved collision names must fail without replacing any file");

    assert!(error
        .to_string()
        .contains("could not choose a safe filename"));
    assert_eq!(writes, 0);
    assert_eq!(quarantines, 0);
    assert_eq!(temporary_opens, 0);
}

#[cfg(unix)]
#[test]
fn native_attachment_save_fails_closed_when_the_descriptor_probe_errors() {
    use std::io::{Error, ErrorKind};

    let directory = tempdir().expect("temporary Downloads directory");
    let mut writes = 0;
    let mut quarantines = 0;
    let mut temporary_opens = 0;
    let mut write = |_: &mut std::fs::File, _: &[u8]| {
        writes += 1;
        Ok(())
    };
    let mut quarantine = |_: &std::fs::File| {
        quarantines += 1;
        Ok(())
    };
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || {
        temporary_opens += 1;
        Ok(())
    };
    let mut before_publish = || Ok(());
    let mut after_publish = || Ok(());

    let error = super::save_private_download_with_directory_descriptor_and_cleanup(
        directory.path(),
        "report.pdf",
        b"must not be written",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        |_, _| {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                "synthetic probe denial",
            ))
        },
        super::unlink_private_download_at,
        |file| file.sync_all(),
    )
    .expect_err("an uncertain collision probe must fail closed");

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(Error::kind),
        Some(ErrorKind::PermissionDenied)
    );
    assert_eq!(writes, 0);
    assert_eq!(quarantines, 0);
    assert_eq!(temporary_opens, 0);
}

#[test]
fn native_attachment_save_removes_partial_file_when_write_or_fsync_fails() {
    use std::io::{Error, ErrorKind, Write};

    let directory = tempdir().expect("temporary Downloads directory");
    let result = super::save_private_download_with_writer(
        directory.path(),
        "fictional.pdf",
        b"fictional full attachment",
        |file, bytes| {
            file.write_all(&bytes[..5])?;
            Err(Error::new(ErrorKind::StorageFull, "synthetic disk full"))
        },
    );

    assert!(
        result.is_err(),
        "a partial write must fail the save operation"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .expect("read Downloads directory")
            .next()
            .is_none(),
        "a failed write must not leave a plausible partial attachment"
    );
}

#[test]
fn native_attachment_save_stops_before_write_when_directory_creation_is_denied() {
    use std::io::{Error, ErrorKind};

    let directory = tempdir().expect("temporary parent directory");
    let downloads = directory.path().join("Downloads");
    let mut opened = false;
    let result = super::save_private_download_with_operations(
        &downloads,
        "fictional.pdf",
        b"attachment",
        |_| {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                "synthetic directory denial",
            ))
        },
        |_| {
            opened = true;
            unreachable!("the destination cannot be opened after directory denial")
        },
        |_, _| unreachable!("the destination cannot be written after directory denial"),
        |_| Ok(()),
    );

    assert_eq!(
        result
            .unwrap_err()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied)
    );
    assert!(!opened);
    assert!(!downloads.exists());
}

#[test]
fn native_attachment_save_stops_before_write_when_destination_open_is_denied() {
    use std::io::{Error, ErrorKind};

    let directory = tempdir().expect("temporary Downloads directory");
    let mut wrote = false;
    let result = super::save_private_download_with_operations(
        directory.path(),
        "fictional.pdf",
        b"attachment",
        |path| std::fs::create_dir_all(path),
        |_| {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                "synthetic destination denial",
            ))
        },
        |_, _| {
            wrote = true;
            unreachable!("a denied destination must not be written")
        },
        |_| Ok(()),
    );

    assert_eq!(
        result
            .unwrap_err()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied)
    );
    assert!(!wrote);
    assert!(std::fs::read_dir(directory.path())
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn native_attachment_save_removes_a_completed_file_when_quarantine_fails() {
    use std::io::Write;

    let directory = tempdir().expect("temporary Downloads directory");
    #[cfg(unix)]
    let result = super::save_private_download_with_directory_descriptor(
        directory.path(),
        "fictional.command",
        b"attachment",
        |file, bytes| file.write_all(bytes).and_then(|_| file.sync_all()),
        |_| Err(anyhow::anyhow!("synthetic quarantine failure")),
        |_| Ok(()),
        || Ok(()),
        || Ok(()),
    );
    #[cfg(not(unix))]
    let result = super::save_private_download_with_operations(
        directory.path(),
        "fictional.command",
        b"attachment",
        |path| std::fs::create_dir_all(path),
        super::open_private_download_file,
        |file, bytes| file.write_all(bytes).and_then(|_| file.sync_all()),
        |_| Err(anyhow::anyhow!("synthetic quarantine failure")),
    );

    assert!(result.is_err());
    assert!(std::fs::read_dir(directory.path())
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn descriptor_relative_save_rejects_a_changed_configured_directory_before_publish() {
    use std::{io::Write, os::unix::fs::symlink};

    let root = tempdir().expect("temporary configured Downloads parent");
    let downloads = root.path().join("Downloads");
    std::fs::create_dir(&downloads).expect("original Downloads directory");
    let original = root.path().join("Downloads-before-swap");
    let outside = tempdir().expect("outside directory");

    let result = super::save_private_download_with_directory_descriptor(
        &downloads,
        "report.pdf",
        b"fictional mail bytes",
        |file, bytes| file.write_all(bytes).and_then(|_| file.sync_all()),
        super::quarantine_download,
        |_| {
            std::fs::rename(&downloads, &original).expect("move opened Downloads directory");
            symlink(outside.path(), &downloads).expect("replace configured path with symlink");
            Ok(())
        },
        || Ok(()),
        || Ok(()),
    );

    assert!(
        result
            .expect_err("a changed configured path must not report an unusable saved path")
            .to_string()
            .contains("Downloads directory changed while saving"),
        "the caller must retry after a configured Downloads directory swap"
    );
    assert!(
        !original.join("report.pdf").exists(),
        "the temporary file is cleaned before the final name can become visible"
    );
    assert!(
        !outside.path().join("report.pdf").exists(),
        "the replacement parent path must never receive attachment bytes"
    );
}

#[cfg(unix)]
#[test]
fn descriptor_relative_cleanup_cannot_delete_an_external_file_after_a_parent_swap() {
    use std::{
        io::{Error, ErrorKind},
        os::unix::fs::symlink,
    };

    let root = tempdir().expect("temporary configured Downloads parent");
    let downloads = root.path().join("Downloads");
    std::fs::create_dir(&downloads).expect("original Downloads directory");
    let original = root.path().join("Downloads-before-swap");
    let outside = tempdir().expect("outside directory");
    let external = outside.path().join("report.pdf");
    std::fs::write(&external, b"outside original").expect("external fixture");

    let result = super::save_private_download_with_directory_descriptor(
        &downloads,
        "report.pdf",
        b"partial mail bytes",
        |_, _| Err(Error::new(ErrorKind::StorageFull, "synthetic disk full")),
        super::quarantine_download,
        |_| Ok(()),
        || {
            std::fs::rename(&downloads, &original).expect("move opened Downloads directory");
            symlink(outside.path(), &downloads).expect("replace configured path with symlink");
            Ok(())
        },
        || Ok(()),
    );

    assert!(result.is_err(), "the write fault must still fail the save");
    assert!(
        !original.join("report.pdf").exists(),
        "the partial attachment is removed from the descriptor-pinned directory"
    );
    assert_eq!(
        std::fs::read(&external).expect("read external fixture"),
        b"outside original",
        "cleanup must not follow the replacement parent path"
    );
}

#[cfg(unix)]
#[test]
fn post_publish_directory_swap_removes_the_pinned_final_file_before_reporting_retry() {
    use std::{io::Write, os::unix::fs::symlink};

    let root = tempdir().expect("temporary configured Downloads parent");
    let downloads = root.path().join("Downloads");
    std::fs::create_dir(&downloads).expect("original Downloads directory");
    let original = root.path().join("Downloads-before-swap");
    let outside = tempdir().expect("outside directory");
    let mut write = |file: &mut std::fs::File, bytes: &[u8]| {
        file.write_all(bytes).and_then(|_| file.sync_all())
    };
    let mut quarantine = super::quarantine_download;
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || Ok(());
    let mut before_publish = || Ok(());
    let mut after_publish = || {
        std::fs::rename(&downloads, &original).expect("move published Downloads directory");
        symlink(outside.path(), &downloads).expect("replace configured path with symlink");
        Ok(())
    };
    let result = super::save_private_download_with_directory_descriptor_and_cleanup(
        &downloads,
        "report.pdf",
        b"published then removed",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        super::private_download_candidate_exists_at,
        super::unlink_private_download_at,
        |directory| directory.sync_all(),
    );

    assert!(result
        .expect_err("the changed configured path must not return a stale success path")
        .to_string()
        .contains("Downloads directory changed while publishing"),);
    assert!(
        !original.join("report.pdf").exists(),
        "the descriptor-relative cleanup removes the already-published file"
    );
    assert!(
        !outside.path().join("report.pdf").exists(),
        "the replacement configured path never receives the file"
    );
}

#[cfg(unix)]
#[test]
fn post_publish_directory_sync_failure_keeps_the_complete_attachment_and_batch_success() {
    use std::io::{Error, Write};

    let directory = tempdir().expect("temporary Downloads directory");
    let mut saved = Vec::new();
    super::save_attachment_batch_step(&mut saved, 1, Ok(()), || {
        let mut write = |file: &mut std::fs::File, bytes: &[u8]| {
            file.write_all(bytes).and_then(|_| file.sync_all())
        };
        let mut quarantine = super::quarantine_download;
        let mut after_directory_open = |_: &std::fs::File| Ok(());
        let mut after_file_open = || Ok(());
        let mut before_publish = || Ok(());
        let mut after_publish = || Ok(());
        super::save_private_download_with_directory_descriptor_and_cleanup(
            directory.path(),
            "report.pdf",
            b"complete attachment",
            &mut write,
            &mut quarantine,
            &mut after_directory_open,
            &mut after_file_open,
            &mut before_publish,
            &mut after_publish,
            super::private_download_candidate_exists_at,
            super::unlink_private_download_at,
            |_| Err(Error::other("synthetic directory sync failure")),
        )
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
    })
    .expect(
        "a published attachment remains a successful batch item despite directory sync warning",
    );

    assert_eq!(saved.len(), 1);
    assert_eq!(
        std::fs::read(directory.path().join("report.pdf")).unwrap(),
        b"complete attachment"
    );
}

#[cfg(unix)]
#[test]
fn failed_temporary_cleanup_reports_the_residual_artifact_without_publishing_a_filename() {
    use std::io::{Error, ErrorKind};

    let directory = tempdir().expect("temporary Downloads directory");
    let mut write = |_: &mut std::fs::File, _: &[u8]| {
        Err(Error::new(ErrorKind::StorageFull, "synthetic disk full"))
    };
    let mut quarantine = |_: &std::fs::File| Ok(());
    let mut after_directory_open = |_: &std::fs::File| Ok(());
    let mut after_file_open = || Ok(());
    let mut before_publish = || Ok(());
    let mut after_publish = || Ok(());
    let result = super::save_private_download_with_directory_descriptor_and_cleanup(
        directory.path(),
        "report.pdf",
        b"partial mail bytes",
        &mut write,
        &mut quarantine,
        &mut after_directory_open,
        &mut after_file_open,
        &mut before_publish,
        &mut after_publish,
        super::private_download_candidate_exists_at,
        |_, _| {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                "synthetic unlink denial",
            ))
        },
        |directory| directory.sync_all(),
    );

    assert!(
        result
            .expect_err("a failed write must not be reported as saved")
            .to_string()
            .contains("a hidden temporary attachment may remain"),
        "the error must disclose cleanup uncertainty"
    );
    assert!(
        !directory.path().join("report.pdf").exists(),
        "a failed save never publishes a plausible final filename"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .expect("read Downloads directory")
            .any(|entry| entry
                .expect("temporary entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".dakia-")),
        "only a clearly hidden temporary artifact can remain after cleanup failure"
    );
}

#[cfg(unix)]
#[test]
fn subprocess_exit_at_the_publish_barrier_never_exposes_a_partial_attachment() {
    const CHILD_DOWNLOADS: &str = "DAKIA_ATTACHMENT_PUBLISH_BARRIER_CHILD_DOWNLOADS";
    if let Ok(downloads) = std::env::var(CHILD_DOWNLOADS) {
        let downloads = std::path::PathBuf::from(downloads);
        let _ = super::save_private_download_with_directory_descriptor(
            &downloads,
            "report.pdf",
            b"fully-written-but-not-published bytes",
            |file, bytes| std::io::Write::write_all(file, bytes).and_then(|_| file.sync_all()),
            super::quarantine_download,
            |_| Ok(()),
            || Ok(()),
            || std::process::exit(73),
        );
        unreachable!("the publish barrier exits the child process");
    }

    let directory = tempdir().expect("temporary Downloads directory");
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "attachment_pipeline_tests::subprocess_exit_at_the_publish_barrier_never_exposes_a_partial_attachment",
            "--nocapture",
        ])
        .env(CHILD_DOWNLOADS, directory.path())
        .status()
        .expect("run child test process");

    assert_eq!(
        status.code(),
        Some(73),
        "child must exit at publish barrier"
    );
    assert!(
        !directory.path().join("report.pdf").exists(),
        "an interrupted save must not expose the final attachment name"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .expect("read Downloads directory")
            .any(|entry| entry
                .expect("temporary entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".dakia-")),
        "an interrupted pre-publish write can leave only a hidden temporary file"
    );
}

#[test]
fn bulk_attachment_save_reports_the_first_durable_file_when_the_second_write_fails() {
    use std::io::{Error, ErrorKind, Write};

    let directory = tempdir().expect("temporary Downloads directory");
    let mut saved = Vec::new();
    super::save_attachment_batch_step(&mut saved, 2, Ok(()), || {
        super::save_private_download(directory.path(), "first.pdf", b"first attachment")
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| error.to_string())
    })
    .expect("first attachment saves");

    let failure = super::save_attachment_batch_step(&mut saved, 2, Ok(()), || {
        super::save_private_download_with_writer(
            directory.path(),
            "second.pdf",
            b"second attachment",
            |file, bytes| {
                file.write_all(&bytes[..4])?;
                Err(Error::new(ErrorKind::StorageFull, "synthetic disk full"))
            },
        )
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
    })
    .expect_err("second attachment must report a partial bulk save");

    assert_eq!(
        std::fs::read(directory.path().join("first.pdf")).unwrap(),
        b"first attachment"
    );
    assert!(
        !directory.path().join("second.pdf").exists(),
        "the failed second write is cleaned up"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&failure).unwrap(),
        serde_json::json!({ "kind": "partial", "savedCount": 1, "totalCount": 2 })
    );
}

#[test]
fn bulk_attachment_save_reports_a_stale_epoch_after_the_first_file_without_saving_more() {
    let directory = tempdir().expect("temporary Downloads directory");
    let mut saved = Vec::new();
    super::save_attachment_batch_step(&mut saved, 2, Ok(()), || {
        super::save_private_download(directory.path(), "first.pdf", b"first attachment")
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| error.to_string())
    })
    .expect("first attachment saves");

    let failure = super::save_attachment_batch_step(
        &mut saved,
        2,
        Err("This message changed. Refresh the mailbox before trying again.".into()),
        || panic!("a stale epoch must prevent the next attachment save"),
    )
    .expect_err("stale identity must return a partial bulk save");

    assert_eq!(
        std::fs::read(directory.path().join("first.pdf")).unwrap(),
        b"first attachment"
    );
    assert!(
        !directory.path().join("second.pdf").exists(),
        "the stale second attachment must never be written"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&failure).unwrap(),
        serde_json::json!({ "kind": "partial", "savedCount": 1, "totalCount": 2 })
    );
}

#[cfg(all(feature = "native-mail-safety-fixture", unix))]
#[test]
fn native_fixture_download_directory_rejects_a_symlink_escape_after_startup() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("fixture root");
    std::fs::write(
        root.path().join(".dakia-native-mail-safety-root"),
        super::NATIVE_MAIL_SAFETY_FIXTURE_MARKER,
    )
    .expect("fixture ownership sentinel");
    let downloads = root.path().join("Downloads");
    std::fs::create_dir(&downloads).expect("fixture Downloads directory");
    assert_eq!(
        super::verified_native_mail_safety_download_directory(root.path(), &downloads)
            .expect("owned fixture Downloads directory"),
        downloads.canonicalize().unwrap()
    );

    let outside = tempdir().expect("outside directory");
    let swapped = root.path().join("SwappedDownloads");
    symlink(outside.path(), &swapped).expect("synthetic swapped path");
    assert!(
        super::verified_native_mail_safety_download_directory(root.path(), &swapped).is_err(),
        "a post-startup symlink cannot redirect email downloads outside the fixture root"
    );
}
