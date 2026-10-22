use super::{
    acquire_existing_profile_migration_lock_until, acquire_profile_migration_lock_until,
    install_store_open_preflight_race_hook, profile_migration_lock_path,
    PROFILE_MIGRATION_LOCK_WAIT,
};
use crate::Store;
use sqlx::sqlite::SqliteConnectOptions;
use std::{
    future::pending,
    str::FromStr,
    time::{Duration, Instant},
};
use tempfile::tempdir;
use tokio::sync::oneshot;

#[tokio::test]
async fn profile_migration_lock_wait_is_bounded_and_restartable() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let first = acquire_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("acquire first migration lock");

    let error = match acquire_profile_migration_lock_until(
        &database,
        Instant::now() + Duration::from_millis(100),
    )
    .await
    {
        Ok(_) => panic!("a competing opener must time out"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("timed out waiting for another Dakia process"),
        "{error:#}"
    );

    drop(first);
    acquire_profile_migration_lock_until(&database, Instant::now() + PROFILE_MIGRATION_LOCK_WAIT)
        .await
        .expect("lock is reusable after the first opener finishes");
}

#[tokio::test]
async fn store_waits_for_an_existing_migration_lock_before_schema_preflight() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    drop(Store::open(&database).await.expect("initialize profile"));

    let options = SqliteConnectOptions::from_str(database.to_str().expect("UTF-8 database path"))
        .expect("parse database path");
    let repair = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .expect("open schema repair connection");
    sqlx::query("DROP TABLE send_attempts")
        .execute(&repair)
        .await
        .expect("remove send journal during simulated migration");
    sqlx::query("CREATE VIEW send_attempts AS SELECT id FROM accounts")
        .execute(&repair)
        .await
        .expect("install transient conflicting schema");

    let migration_lock = acquire_existing_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("inspect existing migration lock")
    .expect("initialized profiles retain a migration lock inode");
    let opener_database = database.clone();
    let mut opener = tokio::spawn(async move { Store::open(opener_database).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut opener)
            .await
            .is_err(),
        "a concurrent opener must not inspect transient migration schema"
    );

    sqlx::query("DROP VIEW send_attempts")
        .execute(&repair)
        .await
        .expect("remove transient schema");
    sqlx::query("CREATE TABLE send_attempts (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, attempt_id TEXT NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('pending', 'submitted', 'delivery_uncertain', 'sent_copy_failed')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, attempt_id))")
        .execute(&repair)
        .await
        .expect("finish simulated migration");
    repair.close().await;
    drop(migration_lock);

    let store = tokio::time::timeout(Duration::from_secs(5), opener)
        .await
        .expect("opener remained bounded")
        .expect("opener task did not panic")
        .expect("opener accepted settled schema");
    drop(store);
}

#[tokio::test]
async fn store_rechecks_a_lock_created_during_the_initial_schema_preflight_race() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let options = SqliteConnectOptions::from_str(database.to_str().expect("UTF-8 database path"))
        .expect("parse database path")
        .create_if_missing(true);
    let fixture = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .expect("create profile without a migration lock");
    sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, data TEXT NOT NULL, created_at TEXT NOT NULL)")
        .execute(&fixture)
        .await
        .expect("create accounts fixture");
    sqlx::query("CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&fixture)
        .await
        .expect("create profile marker fixture");
    sqlx::query("INSERT INTO app_meta(key, value) VALUES ('profile_schema_version', '1')")
        .execute(&fixture)
        .await
        .expect("mark current profile fixture");
    sqlx::query("CREATE TABLE send_attempts (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, attempt_id TEXT NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('pending', 'submitted', 'delivery_uncertain', 'sent_copy_failed')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, attempt_id))")
        .execute(&fixture)
        .await
        .expect("create valid send journal fixture");
    fixture.close().await;
    assert!(!profile_migration_lock_path(&database)
        .expect("fixture lock path")
        .exists());

    let (_hook, after_absent_lookup, continue_to_preflight) =
        install_store_open_preflight_race_hook(database.clone());
    let opener_database = database.clone();
    let mut opener = tokio::spawn(async move { Store::open(opener_database).await });
    after_absent_lookup.wait().await;

    let migration_lock = acquire_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("competing migration creates and acquires the lock");
    let repair = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("open simulated migration connection");
    sqlx::query("DROP TABLE send_attempts")
        .execute(&repair)
        .await
        .expect("remove send journal during simulated migration");
    sqlx::query("CREATE VIEW send_attempts AS SELECT id FROM accounts")
        .execute(&repair)
        .await
        .expect("install transient conflicting schema");

    continue_to_preflight.wait().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut opener)
            .await
            .is_err(),
        "the failed preflight must recheck and wait for the newly created lock"
    );

    sqlx::query("DROP VIEW send_attempts")
        .execute(&repair)
        .await
        .expect("remove transient schema");
    sqlx::query("CREATE TABLE send_attempts (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, attempt_id TEXT NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('pending', 'submitted', 'delivery_uncertain', 'sent_copy_failed')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(account_id, attempt_id))")
        .execute(&repair)
        .await
        .expect("finish simulated migration");
    repair.close().await;
    drop(migration_lock);

    let store = tokio::time::timeout(Duration::from_secs(5), opener)
        .await
        .expect("race-recovery opener remained bounded")
        .expect("race-recovery opener task did not panic")
        .expect("race-recovery opener accepted settled schema");
    drop(store);
}

#[tokio::test]
async fn cancelling_a_lock_owner_releases_the_profile_for_restart() {
    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let owner_database = database.clone();
    let (acquired_tx, acquired_rx) = oneshot::channel();
    let owner = tokio::spawn(async move {
        let _lock = acquire_profile_migration_lock_until(
            &owner_database,
            Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
        )
        .await
        .expect("acquire cancellable migration lock");
        acquired_tx.send(()).expect("publish acquired lock");
        pending::<()>().await;
    });
    acquired_rx.await.expect("lock owner started");

    owner.abort();
    assert!(owner
        .await
        .expect_err("owner task must be cancelled")
        .is_cancelled());

    acquire_profile_migration_lock_until(&database, Instant::now() + PROFILE_MIGRATION_LOCK_WAIT)
        .await
        .expect("cancelled owner releases its OS lock");
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_database_aliases_share_one_migration_lock() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    std::fs::write(&database, []).expect("create database identity file");
    let alias = directory.path().join("dakia-alias.db");
    symlink(&database, &alias).expect("create database symlink alias");

    assert_eq!(
        profile_migration_lock_path(&database).expect("real lock path"),
        profile_migration_lock_path(&alias).expect("alias lock path")
    );
    let first = acquire_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("acquire real-path migration lock");
    let error = match acquire_profile_migration_lock_until(
        &alias,
        Instant::now() + Duration::from_millis(100),
    )
    .await
    {
        Ok(_) => panic!("alias must contend on the same lock inode"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("timed out"), "{error:#}");
    drop(first);

    acquire_profile_migration_lock_until(&alias, Instant::now() + PROFILE_MIGRATION_LOCK_WAIT)
        .await
        .expect("alias opens after the real-path owner exits");
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_database_aliases_share_the_missing_targets_migration_lock() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let relative_alias = directory.path().join("relative-alias.db");
    let absolute_alias = directory.path().join("absolute-alias.db");
    symlink("dakia.db", &relative_alias).expect("create relative dangling database alias");
    symlink(&database, &absolute_alias).expect("create absolute dangling database alias");

    let direct_lock = profile_migration_lock_path(&database).expect("direct lock path");
    assert_eq!(
        profile_migration_lock_path(&relative_alias).expect("relative alias lock path"),
        direct_lock
    );
    assert_eq!(
        profile_migration_lock_path(&absolute_alias).expect("absolute alias lock path"),
        direct_lock
    );

    let first = acquire_profile_migration_lock_until(
        &relative_alias,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("acquire dangling-alias migration lock");
    let error = match acquire_profile_migration_lock_until(
        &database,
        Instant::now() + Duration::from_millis(100),
    )
    .await
    {
        Ok(_) => panic!("direct missing target must contend with its dangling alias"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("timed out"), "{error:#}");
    drop(first);

    acquire_profile_migration_lock_until(
        &absolute_alias,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("absolute alias reuses the missing target lock");
}

#[cfg(unix)]
#[test]
fn profile_migration_lock_rejects_database_symlink_loops() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary profile directory");
    let first = directory.path().join("first.db");
    let second = directory.path().join("second.db");
    symlink("second.db", &first).expect("create first database alias");
    symlink("first.db", &second).expect("create second database alias");

    let error = match profile_migration_lock_path(&first) {
        Ok(_) => panic!("database symlink loop must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("resolve profile database")
            || error.to_string().contains("symlink loop"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("list loop fixture")
            .count(),
        2,
        "rejection must not create a migration lock"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn profile_migration_lock_keeps_one_owner_only_inode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let lock_path = profile_migration_lock_path(&database).expect("lock path");
    let first = acquire_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("acquire first migration lock");
    let first_metadata = std::fs::metadata(&lock_path).expect("inspect first lock inode");
    assert!(first_metadata.is_file());
    assert_eq!(first_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(first_metadata.uid(), unsafe { libc::geteuid() });
    let first_inode = first_metadata.ino();

    drop(first);
    assert!(lock_path.is_file(), "lock inode must persist after unlock");
    let second = acquire_profile_migration_lock_until(
        &database,
        Instant::now() + PROFILE_MIGRATION_LOCK_WAIT,
    )
    .await
    .expect("reacquire persistent migration lock");
    let second_metadata = std::fs::metadata(&lock_path).expect("inspect reused lock inode");
    assert_eq!(second_metadata.ino(), first_inode);
    drop(second);
}

#[cfg(unix)]
#[test]
fn profile_migration_lock_refuses_a_symlink_substitution() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary profile directory");
    let database = directory.path().join("dakia.db");
    let lock_path = profile_migration_lock_path(&database).expect("lock path");
    let target = directory.path().join("untrusted-target");
    std::fs::write(&target, b"preserve").expect("write substitution target");
    symlink(&target, &lock_path).expect("replace lock path with symlink");

    let error = match super::open_profile_migration_lock(&database) {
        Ok(_) => panic!("O_NOFOLLOW must reject the substituted lock"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("open profile migration lock"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(&target).expect("read preserved target"),
        b"preserve"
    );
}
