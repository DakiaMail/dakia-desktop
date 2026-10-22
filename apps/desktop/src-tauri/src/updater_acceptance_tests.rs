//! Isolated acceptance tests for Dakia's production Tauri updater pipeline.
//!
//! Every test generates disposable updater keys, serves fictional artifacts
//! over loopback, and targets only a temporary `.app`. These tests exercise
//! Tauri updater signatures and macOS replacement behavior. They do not stand
//! in for Developer ID signing, notarization, stapling, a public updater feed,
//! or an installed release smoke test.

use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri_plugin_updater::UpdaterExt;

const CURRENT_VERSION: &str = "0.4.4";
const CANDIDATE_VERSION: &str = "0.4.5";
const OLD_MARKER: &[u8] = b"fictional Dakia stable 0.4.4\n";
const NEW_MARKER: &[u8] = b"fictional Dakia candidate 0.4.5\n";
const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const MAX_SERVER_CONNECTIONS: usize = 32;
const SERVER_DEADLINE: Duration = Duration::from_secs(10);
const FIXTURE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(3);

struct SignedFixture {
    _root: tempfile::TempDir,
    pubkey: String,
    archive: Vec<u8>,
    signature: String,
    archive_sha256: String,
}

fn signed_fixture() -> SignedFixture {
    let root = tempfile::tempdir().expect("create updater signing fixture");
    let private_key = root.path().join("fictional-updater.key");
    let public_key = root.path().join("fictional-updater.key.pub");
    let archive_path = root.path().join("Dakia-aarch64.app.tar.gz");

    let cli = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../node_modules/@tauri-apps/cli/tauri.js");
    assert!(
        cli.is_file(),
        "Tauri CLI must be installed before updater acceptance"
    );

    let generated = Command::new("node")
        .arg(&cli)
        .args(["signer", "generate", "--ci", "--password", ""])
        .arg("--write-keys")
        .arg(&private_key)
        .output()
        .expect("generate fictional updater key");
    assert!(
        generated.status.success(),
        "fictional updater key generation failed: {}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let archive = candidate_archive();
    fs::write(&archive_path, &archive).expect("write fictional updater archive");
    let signed = Command::new("node")
        .arg(&cli)
        .args(["signer", "sign", "--password", ""])
        .arg("--private-key-path")
        .arg(&private_key)
        .arg(&archive_path)
        .output()
        .expect("sign fictional updater archive");
    assert!(
        signed.status.success(),
        "fictional updater signing failed: {}",
        String::from_utf8_lossy(&signed.stderr)
    );

    let pubkey = fs::read_to_string(public_key)
        .expect("read fictional updater public key")
        .trim()
        .to_owned();
    let signature = fs::read_to_string(archive_path.with_extension("gz.sig"))
        .expect("read fictional updater signature")
        .trim()
        .to_owned();
    let archive_sha256 = format!("{:x}", Sha256::digest(&archive));

    SignedFixture {
        _root: root,
        pubkey,
        archive,
        signature,
        archive_sha256,
    }
}

fn candidate_archive() -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_archive_file(
        &mut archive,
        "Dakia.app/Contents/MacOS/dakia-desktop",
        b"#!/bin/sh\nexit 0\n",
        0o755,
    );
    append_archive_file(
        &mut archive,
        "Dakia.app/Contents/Resources/updater-acceptance.txt",
        NEW_MARKER,
        0o644,
    );
    let encoder = archive.into_inner().expect("finish candidate tar archive");
    encoder.finish().expect("finish candidate gzip archive")
}

fn append_archive_file(
    archive: &mut tar::Builder<GzEncoder<Vec<u8>>>,
    path: &str,
    contents: &[u8],
    mode: u32,
) {
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    archive
        .append_data(&mut header, path, contents)
        .expect("append candidate archive entry");
}

struct DisposableApp {
    _root: tempfile::TempDir,
    app: PathBuf,
    executable: PathBuf,
    marker: PathBuf,
}

impl DisposableApp {
    fn old() -> Self {
        let root = tempfile::tempdir().expect("create disposable updater app root");
        let app = root.path().join("Dakia Updater Acceptance.app");
        let executable = app
            .join("Contents")
            .join("MacOS")
            .join("dakia-updater-acceptance");
        let marker = app
            .join("Contents")
            .join("Resources")
            .join("updater-acceptance.txt");
        fs::create_dir_all(executable.parent().unwrap()).expect("create fictional app executable");
        fs::create_dir_all(marker.parent().unwrap()).expect("create fictional app resources");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write fictional app executable");
        fs::write(&marker, OLD_MARKER).expect("write stable marker");
        Self {
            _root: root,
            app,
            executable,
            marker,
        }
    }

    fn assert_stable_is_unchanged(&self) {
        assert!(
            self.app.is_dir(),
            "the disposable stable app must still exist"
        );
        assert_eq!(fs::read(&self.marker).unwrap(), OLD_MARKER);
        assert!(self.executable.is_file());
    }
}

#[derive(Clone)]
enum ResponseBody {
    Complete(Vec<u8>),
    Interrupted(Vec<u8>, usize),
    InjectedWriteFailure,
}

#[derive(Clone)]
struct Route {
    status: &'static str,
    content_type: &'static str,
    body: ResponseBody,
}

impl Route {
    fn json(value: serde_json::Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: ResponseBody::Complete(serde_json::to_vec(&value).unwrap()),
        }
    }

    fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/octet-stream",
            body: ResponseBody::Complete(bytes),
        }
    }
}

struct FictionalUpdateServer {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    connection_errors: Arc<Mutex<Vec<String>>>,
    accepted_connections: Arc<(Mutex<usize>, Condvar)>,
    finished_connections: Arc<(Mutex<usize>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FictionalUpdateServer {
    fn start(routes: HashMap<String, Route>, expected_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fictional updater server");
        Self::from_listener(listener, routes, expected_requests)
    }

    fn from_listener(
        listener: TcpListener,
        routes: HashMap<String, Route>,
        expected_requests: usize,
    ) -> Self {
        listener
            .set_nonblocking(true)
            .expect("configure fictional updater server");
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = requests.clone();
        let connection_errors = Arc::new(Mutex::new(Vec::new()));
        let server_connection_errors = connection_errors.clone();
        let accepted_connections = Arc::new((Mutex::new(0usize), Condvar::new()));
        let server_accepted_connections = accepted_connections.clone();
        let finished_connections = Arc::new((Mutex::new(0usize), Condvar::new()));
        let server_finished_connections = finished_connections.clone();
        let routes = Arc::new(routes);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + SERVER_DEADLINE;
            let stop = Arc::new(AtomicBool::new(false));
            let mut workers = Vec::new();
            while server_requests.lock().unwrap().len() < expected_requests
                && Instant::now() < deadline
            {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if workers.len() == MAX_SERVER_CONNECTIONS {
                            drop(stream);
                            continue;
                        }
                        let (accepted, changed) = &*server_accepted_connections;
                        *accepted.lock().unwrap() += 1;
                        changed.notify_all();

                        let routes = routes.clone();
                        let requests = server_requests.clone();
                        let connection_errors = server_connection_errors.clone();
                        let stop = stop.clone();
                        let finished_connections = server_finished_connections.clone();
                        workers.push(thread::spawn(move || {
                            if let Err(error) =
                                handle_request(stream, &routes, &requests, &stop, deadline)
                            {
                                connection_errors.lock().unwrap().push(error.to_string());
                            }
                            let (finished, changed) = &*finished_connections;
                            *finished.lock().unwrap() += 1;
                            changed.notify_all();
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fictional updater server failed: {error}"),
                }
            }
            stop.store(true, Ordering::Release);
            for worker in workers {
                worker.join().expect("join fictional updater connection");
            }
        });
        Self {
            origin: format!("http://{address}"),
            requests,
            connection_errors,
            accepted_connections,
            finished_connections,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> url::Url {
        format!("{}{}", self.origin, path).parse().unwrap()
    }

    fn wait_for_accepted_connections(&self, expected: usize, timeout: Duration) -> bool {
        wait_for_counter(&self.accepted_connections, expected, timeout)
    }

    fn wait_for_finished_connections(&self, expected: usize, timeout: Duration) -> bool {
        wait_for_counter(&self.finished_connections, expected, timeout)
    }

    fn finish(mut self) -> Vec<String> {
        self.thread.take().unwrap().join().unwrap();
        let connection_errors = Arc::try_unwrap(self.connection_errors)
            .unwrap()
            .into_inner()
            .unwrap();
        assert!(
            connection_errors.is_empty(),
            "fictional updater server connection failed: {}",
            connection_errors.join("; ")
        );
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

fn wait_for_counter(
    counter: &Arc<(Mutex<usize>, Condvar)>,
    expected: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    let (counter, changed) = &**counter;
    let mut counter = counter.lock().unwrap();
    while *counter < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let (next, result) = changed.wait_timeout(counter, remaining).unwrap();
        counter = next;
        if result.timed_out() && *counter < expected {
            return false;
        }
    }
    true
}

#[test]
fn fictional_update_server_does_not_let_an_idle_preconnection_block_a_fragmented_request() {
    let mut routes = HashMap::new();
    routes.insert(
        "/latest.json".to_owned(),
        Route::json(json!({"fixture": "ready"})),
    );
    let server = FictionalUpdateServer::start(routes, 1);
    let idle = TcpStream::connect(server.origin.strip_prefix("http://").unwrap())
        .expect("open idle updater preconnection");
    assert!(
        server.wait_for_accepted_connections(1, FIXTURE_PROGRESS_TIMEOUT),
        "the idle preconnection must be accepted before the real request"
    );

    let empty = TcpStream::connect(server.origin.strip_prefix("http://").unwrap())
        .expect("open empty updater preconnection");
    assert!(server.wait_for_accepted_connections(2, FIXTURE_PROGRESS_TIMEOUT));
    drop(empty);

    let mut oversized = TcpStream::connect(server.origin.strip_prefix("http://").unwrap())
        .expect("open oversized updater request");
    assert!(server.wait_for_accepted_connections(3, FIXTURE_PROGRESS_TIMEOUT));
    oversized
        .write_all(&vec![b'A'; MAX_REQUEST_HEADER_BYTES + 1])
        .unwrap();
    oversized.shutdown(Shutdown::Write).unwrap();
    drop(oversized);
    assert!(
        server.wait_for_finished_connections(2, FIXTURE_PROGRESS_TIMEOUT),
        "empty and oversized requests must be rejected without blocking the server"
    );
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "incomplete requests must not count toward the expected updater exchange"
    );

    let mut request = TcpStream::connect(server.origin.strip_prefix("http://").unwrap())
        .expect("open fragmented updater request");
    request
        .write_all(b"GET /latest.json HTTP/1.1\r\nHost: fictional")
        .unwrap();
    assert!(
        server.wait_for_accepted_connections(4, FIXTURE_PROGRESS_TIMEOUT),
        "an idle preconnection must not block acceptance of a real updater request"
    );
    request
        .write_all(b"-updater.test\r\nConnection: close\r\n\r\n")
        .unwrap();
    request
        .set_read_timeout(Some(FIXTURE_PROGRESS_TIMEOUT))
        .unwrap();
    let mut response = Vec::new();
    request.read_to_end(&mut response).unwrap();
    drop(idle);

    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"{\"fixture\":\"ready\"}"));
    assert_eq!(server.finish(), vec!["/latest.json"]);
}

#[test]
#[should_panic(
    expected = "fictional updater server connection failed: injected response write failure"
)]
fn fictional_update_server_fails_closed_when_a_response_write_fails() {
    let mut routes = HashMap::new();
    routes.insert(
        "/must-fail".to_owned(),
        Route {
            status: "200 OK",
            content_type: "application/octet-stream",
            body: ResponseBody::InjectedWriteFailure,
        },
    );
    let server = FictionalUpdateServer::start(routes, 1);
    let mut request = TcpStream::connect(server.origin.strip_prefix("http://").unwrap())
        .expect("open updater write-failure probe");
    request
        .write_all(
            b"GET /must-fail HTTP/1.1\r\nHost: fictional-updater.test\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    assert!(server.wait_for_finished_connections(1, FIXTURE_PROGRESS_TIMEOUT));
    server.finish();
}

fn handle_request(
    mut stream: TcpStream,
    routes: &HashMap<String, Route>,
    requests: &Arc<Mutex<Vec<String>>>,
    stop: &AtomicBool,
    deadline: Instant,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Ok(());
        }
        if request.len() == MAX_REQUEST_HEADER_BYTES {
            return Ok(());
        }
        let remaining = MAX_REQUEST_HEADER_BYTES - request.len();
        let read_capacity = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_capacity]) {
            Ok(0) => return Ok(()),
            Ok(read) => request.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
    let Some(path) = request_path(&request) else {
        return Ok(());
    };
    requests.lock().unwrap().push(path.to_owned());
    let route = routes.get(path).cloned().unwrap_or(Route {
        status: "404 Not Found",
        content_type: "text/plain",
        body: ResponseBody::Complete(b"missing route".to_vec()),
    });
    stream.set_nonblocking(false)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(());
    }
    stream.set_write_timeout(Some(remaining))?;
    match route.body {
        ResponseBody::Complete(body) => {
            write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                route.status,
                route.content_type,
                body.len()
            )
            ?;
            stream.write_all(&body)?;
        }
        ResponseBody::Interrupted(body, announced_length) => {
            write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                route.status, route.content_type, announced_length
            )
            ?;
            stream.write_all(&body)?;
            stream.flush()?;
            let _ = stream.shutdown(Shutdown::Both);
        }
        ResponseBody::InjectedWriteFailure => {
            return Err(io::Error::other("injected response write failure"));
        }
    }
    Ok(())
}

fn request_path(request: &[u8]) -> Option<&str> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&request[..header_end]).ok()?;
    let first_line = headers.split("\r\n").next()?;
    let mut parts = first_line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let version = parts.next()?;
    if method != "GET"
        || !path.starts_with('/')
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
    {
        return None;
    }
    Some(path)
}

fn release_manifest(version: &str, artifact_url: &url::Url, signature: &str) -> serde_json::Value {
    json!({
        "version": version,
        "notes": "Isolated fictional updater acceptance candidate",
        "pub_date": "2026-10-17T00:00:00Z",
        "platforms": {
            "darwin-aarch64": {
                "url": artifact_url,
                "signature": signature
            }
        }
    })
}

fn test_app(pubkey: &str) -> tauri::App<tauri::test::MockRuntime> {
    let mut context = super::test_support::mock_context();
    context.config_mut().plugins.0.insert(
        "updater".to_owned(),
        json!({
            "pubkey": pubkey,
            "endpoints": ["https://fictional-updater.invalid/latest.json"],
            "dangerousInsecureTransportProtocol": true
        }),
    );
    tauri::test::mock_builder()
        .plugin(
            tauri_plugin_updater::Builder::new()
                .pubkey(pubkey)
                .target("darwin-aarch64")
                .build(),
        )
        .build(context)
        .expect("build updater acceptance app")
}

async fn checked_update(
    app: &tauri::App<tauri::test::MockRuntime>,
    disposable: &DisposableApp,
    manifest_url: url::Url,
) -> tauri_plugin_updater::Update {
    let update = app
        .updater_builder()
        .endpoints(vec![manifest_url])
        .expect("accept loopback endpoint in a debug acceptance build")
        .executable_path(&disposable.executable)
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("build production updater pipeline")
        .check()
        .await
        .expect("check fictional update feed")
        .expect("candidate must be newer than 0.4.4");
    assert_eq!(update.current_version, CURRENT_VERSION);
    assert_eq!(update.version, CANDIDATE_VERSION);
    update
}

#[tokio::test(flavor = "multi_thread")]
async fn production_updater_downloads_verifies_and_replaces_only_the_disposable_app() {
    let fixture = signed_fixture();
    let disposable = DisposableApp::old();
    let artifact_path = "/v0.4.5/Dakia-aarch64.app.tar.gz";
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve updater origin");
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let artifact_url: url::Url = format!("{origin}{artifact_path}").parse().unwrap();
    let mut routes = HashMap::new();
    routes.insert(
        "/latest.json".to_owned(),
        Route::json(release_manifest(
            CANDIDATE_VERSION,
            &artifact_url,
            &fixture.signature,
        )),
    );
    routes.insert(
        artifact_path.to_owned(),
        Route::bytes(fixture.archive.clone()),
    );
    let server = FictionalUpdateServer::from_listener(listener, routes, 2);

    let app = test_app(&fixture.pubkey);
    let update = checked_update(&app, &disposable, server.url("/latest.json")).await;
    assert_eq!(
        format!("{:x}", Sha256::digest(&fixture.archive)),
        fixture.archive_sha256
    );
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .expect("install valid fictional candidate");

    assert_eq!(fs::read(&disposable.marker).unwrap(), NEW_MARKER);
    assert!(
        disposable
            .app
            .join("Contents/MacOS/dakia-desktop")
            .is_file(),
        "the exact candidate executable from the signed archive must replace the stable fixture"
    );
    assert!(
        !disposable.executable.exists(),
        "the stable fixture executable must not survive a successful replacement"
    );
    assert_eq!(server.finish(), vec!["/latest.json", artifact_path]);
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_updater_artifact_is_rejected_before_the_old_app_is_changed() {
    let fixture = signed_fixture();
    let disposable = DisposableApp::old();
    let mut tampered = fixture.archive.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x01;
    let (server, manifest_url) = update_server(
        CANDIDATE_VERSION,
        &fixture.signature,
        Route::bytes(tampered),
    );
    let app = test_app(&fixture.pubkey);
    let update = checked_update(&app, &disposable, manifest_url).await;

    let error = update
        .download_and_install(|_, _| {}, || {})
        .await
        .expect_err("tampered artifact must fail signature verification");
    assert!(error.to_string().to_lowercase().contains("signature"));
    disposable.assert_stable_is_unchanged();
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn updater_manifest_with_a_signature_from_another_key_is_rejected_safely() {
    let trusted_fixture = signed_fixture();
    let untrusted_fixture = signed_fixture();
    let disposable = DisposableApp::old();
    assert_eq!(
        trusted_fixture.archive, untrusted_fixture.archive,
        "the signature mismatch must be the only changed input"
    );
    let (server, manifest_url) = update_server(
        CANDIDATE_VERSION,
        &untrusted_fixture.signature,
        Route::bytes(trusted_fixture.archive.clone()),
    );
    let app = test_app(&trusted_fixture.pubkey);
    let update = checked_update(&app, &disposable, manifest_url).await;

    let error = update
        .download_and_install(|_, _| {}, || {})
        .await
        .expect_err("a manifest signature from an untrusted key must fail");
    assert!(error.to_string().to_lowercase().contains("signature"));
    disposable.assert_stable_is_unchanged();
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_updater_download_is_rejected_before_the_old_app_is_changed() {
    let fixture = signed_fixture();
    let disposable = DisposableApp::old();
    let partial = fixture.archive[..fixture.archive.len() / 3].to_vec();
    let (server, manifest_url) = update_server(
        CANDIDATE_VERSION,
        &fixture.signature,
        Route {
            status: "200 OK",
            content_type: "application/octet-stream",
            body: ResponseBody::Interrupted(partial, fixture.archive.len()),
        },
    );
    let app = test_app(&fixture.pubkey);
    let update = checked_update(&app, &disposable, manifest_url).await;

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .expect_err("truncated response must fail before install");
    disposable.assert_stable_is_unchanged();
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_but_incompatible_package_is_rejected_before_the_old_app_is_changed() {
    let fixture = signed_fixture();
    let disposable = DisposableApp::old();
    let invalid_archive = b"signed, but not a macOS app tar archive".to_vec();
    let invalid_root = tempfile::tempdir().unwrap();
    let invalid_path = invalid_root.path().join("incompatible.tar.gz");
    fs::write(&invalid_path, &invalid_archive).unwrap();
    let private_key = fixture._root.path().join("fictional-updater.key");
    sign_file(&invalid_path, &private_key);
    let invalid_signature = fs::read_to_string(invalid_path.with_extension("gz.sig"))
        .unwrap()
        .trim()
        .to_owned();
    let (server, manifest_url) = update_server(
        CANDIDATE_VERSION,
        &invalid_signature,
        Route::bytes(invalid_archive.clone()),
    );
    let app = test_app(&fixture.pubkey);
    let update = checked_update(&app, &disposable, manifest_url).await;

    let downloaded = update
        .download(|_, _| {}, || {})
        .await
        .expect("the generated signature must be valid for the incompatible package");
    assert_eq!(downloaded, invalid_archive);
    update
        .install(downloaded)
        .expect_err("a signed non-app package must fail during extraction");
    disposable.assert_stable_is_unchanged();
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_or_non_newer_manifests_never_request_an_artifact_or_change_the_old_app() {
    let fixture = signed_fixture();
    for (manifest, expected) in [
        (
            json!({"version": "not-semver", "platforms": {}}),
            "malformed",
        ),
        (
            release_manifest(
                CURRENT_VERSION,
                &"http://127.0.0.1:9/must-not-download".parse().unwrap(),
                &fixture.signature,
            ),
            "not-newer",
        ),
    ] {
        let disposable = DisposableApp::old();
        let mut routes = HashMap::new();
        routes.insert("/latest.json".to_owned(), Route::json(manifest));
        let server = FictionalUpdateServer::start(routes, 1);
        let app = test_app(&fixture.pubkey);
        let updater = app
            .updater_builder()
            .endpoints(vec![server.url("/latest.json")])
            .unwrap()
            .executable_path(&disposable.executable)
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let result = updater.check().await;
        match expected {
            "malformed" => {
                assert!(
                    result.is_err(),
                    "a malformed version must reject the manifest"
                );
            }
            "not-newer" => {
                assert!(
                    result
                        .expect("a valid non-newer manifest must parse")
                        .is_none(),
                    "the current release must not be offered as an update"
                );
            }
            _ => unreachable!(),
        }
        disposable.assert_stable_is_unchanged();
        assert_eq!(server.finish(), vec!["/latest.json"]);
    }
}

fn update_server(
    version: &str,
    signature: &str,
    artifact: Route,
) -> (FictionalUpdateServer, url::Url) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve updater origin");
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let artifact_path = "/candidate.tar.gz";
    let artifact_url = format!("{origin}{artifact_path}").parse().unwrap();
    let mut routes = HashMap::new();
    routes.insert(
        "/latest.json".to_owned(),
        Route::json(release_manifest(version, &artifact_url, signature)),
    );
    routes.insert(artifact_path.to_owned(), artifact);
    let server = FictionalUpdateServer::from_listener(listener, routes, 2);
    let manifest_url = server.url("/latest.json");
    (server, manifest_url)
}

fn sign_file(path: &Path, private_key: &Path) {
    let cli = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../node_modules/@tauri-apps/cli/tauri.js");
    let output = Command::new("node")
        .arg(cli)
        .args(["signer", "sign", "--password", ""])
        .arg("--private-key-path")
        .arg(private_key)
        .arg(path)
        .output()
        .expect("sign incompatible fictional updater archive");
    assert!(
        output.status.success(),
        "fictional updater signing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
