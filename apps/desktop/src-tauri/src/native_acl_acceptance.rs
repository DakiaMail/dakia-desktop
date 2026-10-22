//! Native WebKit acceptance for Tauri's command capability boundary.
//!
//! This module is compiled only into the opt-in fictional-mailbox build. It
//! creates an unlisted, hidden webview at an owned loopback origin and asks
//! that page to invoke production commands through the real Wry IPC bridge.
//! No capability grants this window access. The evidence therefore passes
//! only when every safely dispatchable application command is rejected by
//! Tauri's ACL before its handler runs, while the normal main window can still
//! invoke a read-only command through the same native bridge. Side-effectful
//! no-argument commands are checked against the embedded capability scopes and
//! are never dispatched by the probe. A second tier supplies complete
//! fictional payloads for mail mutation, attachment saving, URL opening,
//! notifications, and window creation, including selected plugins.

#[cfg(target_os = "macos")]
use super::native_mail_safety_data_store_identifier;
use super::{load_message_content, AppState, NATIVE_MAIL_SAFETY_FIXTURE_MARKER};
use base64::{engine::general_purpose::STANDARD, Engine};
use dakia_core::SearchQuery;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use uuid::Uuid;

const PROBE_FLAG: &str = "DAKIA_NATIVE_MAIL_SAFETY_ACL_PROBE";
const REMOTE_PROBE_WINDOW_LABEL: &str = "native-acl-remote-probe";
const LOCAL_PROBE_WINDOW_LABEL: &str = "native-acl-local-probe";
const EVIDENCE_FILENAME: &str = "native-acl-acceptance.json";
const TARGET_SUBJECT: &str = "Remote content safety check";
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 48 * 1024;
const TARGET_WAIT: Duration = Duration::from_secs(120);
const RESULT_WAIT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_PROBE_CONNECTIONS: usize = 16;
const DENIED_COMMANDS: [&str; 8] = [
    "set_message_read",
    "save_attachment",
    "open_external_url",
    "send_desktop_notification",
    "open_settings_window",
    "plugin:webview|create_webview_window",
    "plugin:opener|open_url",
    "plugin:notification|notify",
];
const STATIC_ONLY_COMMANDS: [&str; 7] = [
    "install_terminal_command",
    "remove_terminal_command",
    "open_account_window",
    "classify_pending",
    "start_realtime_sync",
    "reconcile_realtime_sync",
    "quit_startup_recovery",
];
const READ_ONLY_NO_ARGUMENT_COMMANDS: [&str; 6] = [
    "provider_presets",
    "terminal_command_status",
    "accounts",
    "realtime_sync_status",
    "mail_rebuild_status",
    "translation_models",
];
const RUNTIME_COMMANDS: [&str; 52] = [
    "provider_presets",
    "message_content",
    "configure_tray",
    "terminal_command_status",
    "message_attachments",
    "save_attachment",
    "export_message",
    "save_all_attachments",
    "forward_attachments",
    "read_dropped_files",
    "accounts",
    "update_account",
    "show_account_context_menu",
    "show_email_address_context_menu",
    "remove_account",
    "open_external_url",
    "open_compose_window",
    "list_send_recoveries",
    "open_send_recovery_window",
    "read_send_recovery_seed",
    "acknowledge_send_recovery_seed",
    "open_reader_window",
    "open_settings_window",
    "add_account",
    "search",
    "search_smart_inbox",
    "conversation_for_target",
    "search_remote",
    "set_message_category",
    "set_message_starred",
    "set_message_read",
    "starred_conversation_count",
    "realtime_sync_status",
    "record_notification_delivered",
    "hydrate_message",
    "mail_rebuild_status",
    "sync_account",
    "send_message",
    "apply_mailbox_action",
    "unsubscribe_message",
    "trash_messages_from_sender",
    "ai_summarize",
    "ai_draft",
    "ai_available",
    "send_desktop_notification",
    "set_ai_api_key",
    "translation_models",
    "translation_model_files",
    "translation_detect_language",
    "translation_install_model",
    "translation_cancel_install",
    "translation_remove_model",
];
const CAPABILITY_FILES: [(&str, &str); 8] = [
    ("account.json", include_str!("../capabilities/account.json")),
    ("compose.json", include_str!("../capabilities/compose.json")),
    ("default.json", include_str!("../capabilities/default.json")),
    (
        "fatal-recovery.json",
        include_str!("../capabilities/fatal-recovery.json"),
    ),
    (
        "notification-non-macos.json",
        include_str!("../capabilities/notification-non-macos.json"),
    ),
    (
        "notification-permission-macos.json",
        include_str!("../capabilities/notification-permission-macos.json"),
    ),
    ("reader.json", include_str!("../capabilities/reader.json")),
    (
        "settings.json",
        include_str!("../capabilities/settings.json"),
    ),
];

fn registered_command_inventory() -> anyhow::Result<Vec<String>> {
    let source = include_str!("lib.rs");
    let marker = ".invoke_handler(tauri::generate_handler![";
    let body = source
        .split_once(marker)
        .and_then(|(_, tail)| tail.split_once("])"))
        .map(|(body, _)| body)
        .ok_or_else(|| anyhow::anyhow!("native ACL probe could not locate the invoke handler"))?;
    let commands = body
        .split(',')
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if commands.is_empty()
        || commands.iter().any(|command| {
            !command
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
        })
    {
        anyhow::bail!("native ACL probe found an invalid invoke-handler inventory");
    }
    let unique = commands.iter().collect::<BTreeSet<_>>();
    if unique.len() != commands.len() {
        anyhow::bail!("native ACL probe found duplicate invoke-handler commands");
    }
    Ok(commands)
}

fn permission_command_inventory() -> anyhow::Result<BTreeSet<String>> {
    let mut commands = BTreeSet::new();
    let mut in_command_list = false;
    for line in include_str!("../permissions/mail-commands.toml").lines() {
        let line = line.trim();
        if line == "commands.allow = [" {
            in_command_list = true;
            continue;
        }
        if in_command_list && line == "]" {
            in_command_list = false;
            continue;
        }
        if in_command_list {
            let command = line
                .strip_prefix('"')
                .and_then(|value| value.split_once('"'))
                .map(|(command, _)| command)
                .ok_or_else(|| anyhow::anyhow!("native ACL permission inventory is malformed"))?;
            if !command
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
            {
                anyhow::bail!("native ACL permission inventory contains an invalid command");
            }
            commands.insert(command.to_owned());
        }
    }
    if in_command_list || commands.is_empty() {
        anyhow::bail!("native ACL permission inventory is incomplete");
    }
    Ok(commands)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remainder = value;
    if let Some(first) = parts.first().filter(|first| !first.is_empty()) {
        let Some(stripped) = remainder.strip_prefix(first) else {
            return false;
        };
        remainder = stripped;
    }
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            continue;
        }
        if index == parts.len() - 1 && !pattern.ends_with('*') {
            return remainder.ends_with(part);
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
    }
    true
}

fn validate_static_probe_scopes(
    registered_commands: &[String],
    permission_commands: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let registered = registered_commands
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for command in STATIC_ONLY_COMMANDS {
        if !registered.contains(command) {
            anyhow::bail!("static-only ACL command is not a registered handler");
        }
        if !permission_commands.contains(command) {
            anyhow::bail!("static-only ACL command lacks a declared application permission");
        }
    }
    for command in READ_ONLY_NO_ARGUMENT_COMMANDS {
        if !registered.contains(command) || !RUNTIME_COMMANDS.contains(&command) {
            anyhow::bail!("read-only no-argument ACL command inventory drifted");
        }
    }
    let covered = RUNTIME_COMMANDS
        .into_iter()
        .chain(STATIC_ONLY_COMMANDS)
        .collect::<BTreeSet<_>>();
    if covered != registered {
        anyhow::bail!(
            "native ACL runtime/static command partition drifted from the handler inventory"
        );
    }
    for (filename, source) in CAPABILITY_FILES {
        let capability: Value = serde_json::from_str(source)?;
        if capability.get("remote").is_some() {
            anyhow::bail!("capability {filename} has an unexpected remote context");
        }
        for selector_key in ["windows", "webviews"] {
            let Some(selectors) = capability.get(selector_key) else {
                if selector_key == "windows" {
                    anyhow::bail!("capability {filename} has no window scope");
                }
                continue;
            };
            let selectors = selectors.as_array().ok_or_else(|| {
                anyhow::anyhow!("capability {filename} has a malformed {selector_key} scope")
            })?;
            for selector in selectors {
                let selector = selector.as_str().ok_or_else(|| {
                    anyhow::anyhow!("capability {filename} has a non-string {selector_key} scope")
                })?;
                if wildcard_matches(selector, LOCAL_PROBE_WINDOW_LABEL)
                    || wildcard_matches(selector, REMOTE_PROBE_WINDOW_LABEL)
                {
                    anyhow::bail!("capability {filename} includes a native ACL probe context");
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct OwnedFixturePaths {
    root: PathBuf,
    profile: PathBuf,
    downloads: PathBuf,
}

impl OwnedFixturePaths {
    fn from_environment() -> anyhow::Result<Option<Self>> {
        match std::env::var(PROBE_FLAG) {
            Ok(value) if value == "1" => {}
            Ok(_) => anyhow::bail!("native ACL probe flag must equal 1"),
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error.into()),
        }

        let required = |name: &str| {
            std::env::var(name)
                .map_err(|_| anyhow::anyhow!("{name} is required for the native ACL probe"))
        };
        if required("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE")? != "1" {
            anyhow::bail!("native ACL probe requires the fictional mail-safety fixture");
        }
        let root = canonical_directory(&required("DAKIA_NATIVE_MAIL_SAFETY_ROOT")?, "root")?;
        require_marker(&root, ".dakia-native-mail-safety-root")?;
        let profile =
            canonical_directory(&required("DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR")?, "profile")?;
        require_marker(&profile, ".dakia-native-mail-safety-profile")?;
        let downloads = canonical_directory(
            &required("DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR")?,
            "Downloads",
        )?;
        if profile == root || !profile.starts_with(&root) {
            anyhow::bail!("native ACL probe profile must be inside its owned root");
        }
        if downloads == root || !downloads.starts_with(&root) {
            anyhow::bail!("native ACL probe Downloads must be inside its owned root");
        }
        Ok(Some(Self {
            root,
            profile,
            downloads,
        }))
    }
}

fn canonical_directory(value: &str, description: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() || !path.is_dir() {
        anyhow::bail!("native ACL probe {description} must be an existing absolute directory");
    }
    path.canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize native ACL probe {description}: {error}"))
}

fn require_marker(directory: &Path, filename: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(directory.join(filename))
        .ok()
        .as_deref()
        != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native ACL probe directory lacks its exact ownership sentinel");
    }
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeTarget {
    nonce: String,
    account_id: String,
    message_id: String,
    attachment_id: String,
    expected_uid_validity: i64,
    recovery_attempt_id: String,
    recovery_key: String,
    opener_url: String,
}

/// Every production handler is exercised from the unlisted local WebView.
/// Recovery commands get structurally complete, fictional arguments so an ACL
/// regression cannot be mistaken for a command deserialization failure.
fn inventory_probe_specs(commands: &[String], target: &ProbeTarget) -> Vec<Value> {
    commands
        .iter()
        .map(|command| {
            let args = match command.as_str() {
                "list_send_recoveries" => json!({
                    "accountIds": [target.account_id],
                    "cursor": null,
                }),
                "open_send_recovery_window" => json!({
                    "accountId": target.account_id,
                    "attemptId": target.recovery_attempt_id,
                }),
                "read_send_recovery_seed" | "acknowledge_send_recovery_seed" => json!({
                    "recoveryKey": target.recovery_key,
                }),
                _ => json!({}),
            };
            json!({ "command": command, "args": args })
        })
        .collect()
}

#[derive(Default)]
struct ProbeHttpState {
    target: Option<ProbeTarget>,
    main_result: Option<Value>,
    local_result: Option<Value>,
    remote_result: Option<Value>,
    opener_canary_hits: usize,
    malformed_requests: usize,
}

struct ActiveProbeConnection(Arc<AtomicUsize>);

impl Drop for ActiveProbeConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ProbeServer {
    origin: String,
    state: Arc<Mutex<ProbeHttpState>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ProbeServer {
    fn start(nonce: String) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            anyhow::bail!("native ACL probe server did not bind loopback");
        }
        let origin = format!("http://127.0.0.1:{}", address.port());
        let state = Arc::new(Mutex::new(ProbeHttpState::default()));
        let thread_state = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_origin = origin.clone();
        let active_connections = Arc::new(AtomicUsize::new(0));
        let thread = thread::Builder::new()
            .name("dakia-native-acl-probe".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if active_connections
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                                    (active < MAX_ACTIVE_PROBE_CONNECTIONS)
                                        .then_some(active + 1)
                                })
                                .is_err()
                            {
                                tracing::warn!("native ACL probe connection limit reached");
                                if let Ok(mut state) = thread_state.lock() {
                                    state.malformed_requests += 1;
                                }
                                continue;
                            }
                            let connection_guard =
                                ActiveProbeConnection(active_connections.clone());
                            let request_state = thread_state.clone();
                            let request_origin = thread_origin.clone();
                            let request_nonce = nonce.clone();
                            if let Err(error) = thread::Builder::new()
                                .name("dakia-native-acl-request".into())
                                .spawn(move || {
                                    let _connection_guard = connection_guard;
                                    if let Err(error) = handle_request(
                                        stream,
                                        &request_origin,
                                        &request_nonce,
                                        &request_state,
                                    ) {
                                        if is_idle_connection_timeout(&error) {
                                            tracing::trace!(
                                                "native ACL probe idle connection expired"
                                            );
                                        } else {
                                            tracing::warn!(error = %error, "native ACL probe request failed");
                                            if let Ok(mut state) = request_state.lock() {
                                                state.malformed_requests += 1;
                                            }
                                        }
                                    }
                                })
                            {
                                tracing::warn!(error = %error, "could not start native ACL request handler");
                                if let Ok(mut state) = thread_state.lock() {
                                    state.malformed_requests += 1;
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "native ACL probe listener failed");
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            origin,
            state,
            stop,
            thread: Some(thread),
        })
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn is_idle_connection_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HttpRequest {
    method: String,
    target: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            anyhow::bail!("native ACL probe request ended before its headers");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            anyhow::bail!("native ACL probe request exceeded its bounded size");
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
    }
    let headers = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("native ACL probe request line is missing"))?
        .split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    if request_line.next() != Some("HTTP/1.1") {
        anyhow::bail!("native ACL probe accepts HTTP/1.1 only");
    }
    let mut content_length = 0_usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse()?;
            }
        }
    }
    if content_length > MAX_RESULT_BYTES || header_end + content_length > MAX_REQUEST_BYTES {
        anyhow::bail!("native ACL probe body exceeded its bounded size");
    }
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            anyhow::bail!("native ACL probe request body was truncated");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            anyhow::bail!("native ACL probe request exceeded its bounded size");
        }
    }
    Ok(HttpRequest {
        method,
        target,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn handle_request(
    mut stream: TcpStream,
    origin: &str,
    nonce: &str,
    state: &Arc<Mutex<ProbeHttpState>>,
) -> anyhow::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(&mut stream)?;
    let probe_path = format!("/probe?nonce={nonce}");
    let config_path = format!("/config?nonce={nonce}");
    let result_path = format!("/result?nonce={nonce}");
    let opener_path = format!("/opener-canary?nonce={nonce}");
    if request.method == "GET" {
        if let Some(result) = query_result(&request.target, origin, nonce, "/main-result")? {
            store_result(state, ProbeResultSlot::Main, result)?;
            respond(&mut stream, 204, "image/gif", b"")?;
            return Ok(());
        }
        if let Some(result) = query_result(&request.target, origin, nonce, "/local-result")? {
            store_result(state, ProbeResultSlot::Local, result)?;
            respond(&mut stream, 204, "image/gif", b"")?;
            return Ok(());
        }
    }
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", target) if target == probe_path => {
            let body = probe_html(nonce);
            respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                body.as_bytes(),
            )?;
        }
        ("GET", target) if target == config_path => {
            let target = state
                .lock()
                .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
                .target
                .clone();
            match target {
                Some(target) => respond_json(&mut stream, 200, &serde_json::to_value(target)?)?,
                None => respond_json(&mut stream, 202, &json!({ "ready": false }))?,
            }
        }
        ("POST", target) if target == result_path => {
            let result: Value = serde_json::from_slice(&request.body)?;
            if result.get("nonce").and_then(Value::as_str) != Some(nonce) {
                respond_json(&mut stream, 403, &json!({ "accepted": false }))?;
                return Ok(());
            }
            store_result(state, ProbeResultSlot::Remote, result)?;
            respond_json(&mut stream, 200, &json!({ "accepted": true }))?;
        }
        ("GET", target) if target == opener_path => {
            state
                .lock()
                .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
                .opener_canary_hits += 1;
            respond(&mut stream, 204, "text/plain; charset=utf-8", b"")?;
        }
        _ => respond(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            b"not found\n",
        )?,
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ProbeResultSlot {
    Main,
    Local,
    Remote,
}

fn store_result(
    state: &Arc<Mutex<ProbeHttpState>>,
    slot: ProbeResultSlot,
    result: Value,
) -> anyhow::Result<()> {
    let mut locked = state
        .lock()
        .map_err(|_| anyhow::anyhow!("probe state poisoned"))?;
    let destination = match slot {
        ProbeResultSlot::Main => &mut locked.main_result,
        ProbeResultSlot::Local => &mut locked.local_result,
        ProbeResultSlot::Remote => &mut locked.remote_result,
    };
    if destination.is_some() {
        anyhow::bail!("native ACL probe result was already recorded");
    }
    *destination = Some(result);
    Ok(())
}

fn query_result(
    target: &str,
    origin: &str,
    nonce: &str,
    expected_path: &str,
) -> anyhow::Result<Option<Value>> {
    let url = url::Url::parse(&format!("{origin}{target}"))?;
    if url.path() != expected_path {
        return Ok(None);
    }
    let mut request_nonce = None;
    let mut encoded = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "nonce" if request_nonce.is_none() => request_nonce = Some(value.into_owned()),
            "data" if encoded.is_none() => encoded = Some(value.into_owned()),
            _ => {}
        }
    }
    if request_nonce.as_deref() != Some(nonce) {
        anyhow::bail!("native ACL probe image result used the wrong nonce");
    }
    let encoded = encoded.ok_or_else(|| anyhow::anyhow!("native ACL probe result is missing"))?;
    let decoded = STANDARD.decode(encoded)?;
    if decoded.len() > MAX_RESULT_BYTES {
        anyhow::bail!("native ACL probe result exceeded its bounded size");
    }
    Ok(Some(serde_json::from_slice(&decoded)?))
}

fn respond_json(stream: &mut TcpStream, status: u16, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    respond(stream, status, "application/json; charset=utf-8", &body)
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; connect-src 'self'\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn probe_html(nonce: &str) -> String {
    let nonce = serde_json::to_string(nonce).expect("probe nonce serializes");
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>Dakia native ACL probe</title>
<script>
(() => {{
  const nonce = {nonce};
  const bounded = value => String(value).slice(0, 1024);
  const pause = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
  const post = async payload => {{
    await fetch(`/result?nonce=${{encodeURIComponent(nonce)}}`, {{
      method: 'POST',
      headers: {{ 'content-type': 'application/json' }},
      body: JSON.stringify({{ nonce, ...payload }}),
    }});
  }};
  const loadTarget = async () => {{
    for (let attempt = 0; attempt < 600; attempt += 1) {{
      const response = await fetch(`/config?nonce=${{encodeURIComponent(nonce)}}`, {{ cache: 'no-store' }});
      if (response.status === 200) return response.json();
      await pause(200);
    }}
    throw new Error('timed out waiting for the exact fictional provider target');
  }};
  const run = async () => {{
    const invoke = window.__TAURI_INTERNALS__?.invoke;
    if (typeof invoke !== 'function') {{
      await post({{ ipcAvailable: false, probes: [] }});
      return;
    }}
    const target = await loadTarget();
    const specs = [
      ['set_message_read', {{ messageId: target.messageId, read: true, expectedUidValidity: target.expectedUidValidity }}],
      ['save_attachment', {{ messageId: target.messageId, attachmentId: target.attachmentId, expectedUidValidity: target.expectedUidValidity }}],
      ['open_external_url', {{ url: target.openerUrl }}],
      ['send_desktop_notification', {{ notification: {{ title: 'Fictional ACL canary', body: 'This notification must be denied', accountId: '62a9320c-04f5-47c2-b4fa-d4952f0b6121', messageId: target.messageId, rfcMessageId: '<fictional-provider-reader-acceptance@localhost>', threadId: 'fictional-provider-reader-thread@localhost', count: 1, sound: null }} }}],
      ['open_settings_window', {{ accountId: null }}],
      ['plugin:webview|create_webview_window', {{ options: {{ label: 'native-acl-forged', url: '/?view=settings', title: 'Forged settings', width: 640, height: 480 }} }}],
      ['plugin:opener|open_url', {{ url: target.openerUrl, with: null }}],
      ['plugin:notification|notify', {{ options: {{ title: 'Fictional plugin ACL canary', body: 'This notification must be denied' }} }}],
    ];
    const probes = [];
    for (const [command, args] of specs) {{
      try {{
        const value = await invoke(command, args);
        probes.push({{ command, outcome: 'resolved', value }});
      }} catch (error) {{
        probes.push({{ command, outcome: 'rejected', error: bounded(error) }});
      }}
    }}
    await post({{ ipcAvailable: true, href: location.href, probes }});
  }};
  run().catch(error => post({{ ipcAvailable: typeof window.__TAURI_INTERNALS__?.invoke === 'function', fatalError: bounded(error), probes: [] }}));
}})();
</script>
<p>Isolated native ACL acceptance probe.</p>
"#
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutableEvidence {
    path: String,
    sha256: String,
    pid: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SideEffectEvidence {
    message_unchanged: bool,
    downloads_unchanged: bool,
    opener_canary_hits: usize,
    settings_window_absent: bool,
    forged_window_absent: bool,
}

fn executable_evidence() -> anyhow::Result<ExecutableEvidence> {
    let path = std::env::current_exe()?.canonicalize()?;
    let mut file = File::open(&path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(ExecutableEvidence {
        path: path.to_string_lossy().into_owned(),
        sha256: format!("{:x}", digest.finalize()),
        pid: std::process::id(),
    })
}

#[derive(Eq, PartialEq)]
struct DownloadEntrySnapshot {
    kind: &'static str,
    length: u64,
    sha256: Option<String>,
}

fn directory_snapshot(path: &Path) -> anyhow::Result<BTreeMap<String, DownloadEntrySnapshot>> {
    let mut snapshot = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let metadata = entry.metadata()?;
        let (kind, sha256) = if file_type.is_file() {
            let mut file = File::open(entry.path())?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
            ("file", Some(format!("{:x}", digest.finalize())))
        } else if file_type.is_dir() {
            ("directory", None)
        } else if file_type.is_symlink() {
            ("symlink", None)
        } else {
            ("other", None)
        };
        snapshot.insert(
            entry.file_name().to_string_lossy().into_owned(),
            DownloadEntrySnapshot {
                kind,
                length: metadata.len(),
                sha256,
            },
        );
    }
    Ok(snapshot)
}

#[cfg(target_os = "macos")]
fn publish_evidence_without_replacement(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both paths are NUL-terminated for the duration of the call. RENAME_EXCL
    // makes the publish atomic while refusing to replace earlier evidence.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn publish_evidence_without_replacement(source: &Path, destination: &Path) -> io::Result<()> {
    // Creating the final hard link is atomic and fails when the destination
    // already exists. The temporary link is removed after publication.
    std::fs::hard_link(source, destination)
}

fn write_evidence(path: &Path, value: &Value) -> anyhow::Result<()> {
    write_evidence_before_publish(path, value, |_| Ok(()))
}

fn write_evidence_before_publish<F>(
    path: &Path,
    value: &Value,
    before_publish: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&Path) -> anyhow::Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("native ACL evidence path lacks a parent"))?;
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("native ACL evidence path lacks a filename"))?
        .to_string_lossy();
    let temporary_path = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4().simple()));
    let mut serialized = serde_json::to_vec_pretty(value)?;
    serialized.push(b'\n');
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_path)?;
    let result = (|| -> anyhow::Result<()> {
        output.write_all(&serialized)?;
        output.sync_all()?;
        drop(output);
        before_publish(&temporary_path)?;
        publish_evidence_without_replacement(&temporary_path, path)?;
        if let Err(error) = std::fs::remove_file(&temporary_path) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

async fn wait_for_target(
    state: &Arc<AppState>,
    account_id: Uuid,
) -> anyhow::Result<(dakia_core::MailSummary, String)> {
    let deadline = tokio::time::Instant::now() + TARGET_WAIT;
    let mut last_content_error = None;
    loop {
        let messages = state
            .store
            .search(&SearchQuery {
                text: TARGET_SUBJECT.into(),
                account_ids: vec![account_id],
                mailbox: Some("INBOX".into()),
                from: None,
                unread_only: false,
                read_only: false,
                flagged_only: false,
                unflagged_only: false,
                category: None,
                limit: Some(10),
                cursor: None,
            })
            .await?;
        if let Some(message) = messages.into_iter().find(|message| {
            message.subject == TARGET_SUBJECT && message.mailbox_uid_validity.is_some()
        }) {
            match load_message_content(state, &message.id, message.mailbox_uid_validity).await {
                Ok(content) => {
                    if let Some(attachment) = content.attachments.first() {
                        return Ok((message, attachment.id.clone()));
                    }
                    last_content_error = Some("fictional target had no attachment".to_owned());
                }
                Err(error) => last_content_error = Some(error),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the exact fictional provider message and attachment: {}",
                last_content_error
                    .as_deref()
                    .unwrap_or("message not synchronized")
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn main_bridge_control<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    nonce: &str,
    collector_origin: &str,
    state: &Arc<Mutex<ProbeHttpState>>,
) -> anyhow::Result<Value> {
    let main = app
        .get_webview_window("main")
        .ok_or_else(|| anyhow::anyhow!("main webview is missing"))?;
    let collector = format!("{collector_origin}/main-result?nonce={nonce}&data=");
    let collector_json = serde_json::to_string(&collector)?;
    let script = format!(
        r#"(() => {{
  if (window.__dakiaNativeAclControlStarted) return;
  window.__dakiaNativeAclControlStarted = true;
  const collector = {collector_json};
  const encode = value => btoa(String.fromCharCode(...new TextEncoder().encode(JSON.stringify(value))));
  const report = value => {{
    const image = document.createElement('img');
    image.hidden = true;
    image.src = collector + encodeURIComponent(encode(value));
    (window.__dakiaNativeAclReportImages ||= []).push(image);
  }};
  const invoke = window.__TAURI_INTERNALS__?.invoke;
  if (typeof invoke !== 'function') {{ report({{ ipcAvailable: false }}); return; }}
  invoke('mail_rebuild_status', {{}}).then(
    value => report({{ ipcAvailable: true, outcome: 'resolved', value }}),
    error => report({{ ipcAvailable: true, outcome: 'rejected', error: String(error).slice(0, 1024) }}),
  );
}})();"#
    );
    let deadline = tokio::time::Instant::now() + RESULT_WAIT;
    loop {
        main.eval(&script)?;
        if let Some(result) = state
            .lock()
            .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
            .main_result
            .clone()
        {
            return Ok(result);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for main-window Wry IPC control");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn dispatcher_acl_passed(result: &Value) -> bool {
    result.get("ipcAvailable").and_then(Value::as_bool) == Some(true)
        && result.get("fatalError").is_none()
        && result
            .get("probes")
            .and_then(Value::as_array)
            .is_some_and(|probes| {
                probes.len() == DENIED_COMMANDS.len()
                    && probes.iter().zip(DENIED_COMMANDS).all(|(probe, command)| {
                        probe.get("command").and_then(Value::as_str) == Some(command)
                            && probe.get("outcome").and_then(Value::as_str) == Some("rejected")
                            && probe
                                .get("error")
                                .and_then(Value::as_str)
                                .is_some_and(|error| registered_acl_denial(command, error))
                    })
            })
}

fn inventory_acl_passed(
    result: &Value,
    registered_commands: &[String],
    permission_commands: &BTreeSet<String>,
) -> bool {
    result
        .get("inventoryProbes")
        .and_then(Value::as_array)
        .is_some_and(|probes| {
            probes.len() == registered_commands.len()
                && probes
                    .iter()
                    .zip(registered_commands)
                    .all(|(probe, command)| {
                        probe.get("command").and_then(Value::as_str) == Some(command.as_str())
                            && probe.get("outcome").and_then(Value::as_str) == Some("rejected")
                            && probe
                                .get("error")
                                .and_then(Value::as_str)
                                .is_some_and(|error| {
                                    registered_acl_denial(command, error)
                                        || (!permission_commands.contains(command)
                                            && error
                                                == format!(
                                                    "{command} not allowed. Command not found"
                                                ))
                                })
                    })
        })
}

fn registered_acl_denial(command: &str, error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    if lower.contains("command not found") || lower.contains("plugin not found") {
        return false;
    }

    // Release builds deliberately return this bounded generic message. Debug
    // builds include the registered command's capability context instead.
    if error == format!("Command {command} not allowed by ACL") {
        return true;
    }

    let pretty_command = command
        .strip_prefix("plugin:")
        .and_then(|value| value.split_once('|'))
        .map_or_else(
            || command.to_owned(),
            |(plugin, command)| format!("{plugin}.{command}"),
        );
    if !error.starts_with(&pretty_command) {
        return false;
    }

    lower.contains(" explicitly denied on origin ")
        || lower.contains(" not allowed on window ")
        || lower.contains(" not allowed on origin [")
        || lower.contains(" not allowed. permissions associated with this command:")
}

fn remote_isolation_passed(result: &Value) -> bool {
    (result.get("ipcAvailable").and_then(Value::as_bool) == Some(false)
        && result.get("fatalError").is_none()
        && result
            .get("probes")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty))
        || dispatcher_acl_passed(result)
}

fn main_control_passed(result: &Value) -> bool {
    result.get("ipcAvailable").and_then(Value::as_bool) == Some(true)
        && result.get("outcome").and_then(Value::as_str) == Some("resolved")
        && result.get("value").is_some_and(Value::is_array)
}

async fn local_dispatcher_probe<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    target: &ProbeTarget,
    collector_origin: &str,
    state: &Arc<Mutex<ProbeHttpState>>,
    registered_commands: &[String],
) -> anyhow::Result<Value> {
    let window = app
        .get_webview_window(LOCAL_PROBE_WINDOW_LABEL)
        .ok_or_else(|| anyhow::anyhow!("local ACL probe webview is missing"))?;
    let collector = format!(
        "{collector_origin}/local-result?nonce={}&data=",
        target.nonce
    );
    let collector_json = serde_json::to_string(&collector)?;
    let target_json = serde_json::to_string(target)?;
    let inventory_json =
        serde_json::to_string(&inventory_probe_specs(registered_commands, target))?;
    let script = format!(
        r#"(() => {{
  if (window.__dakiaNativeLocalAclStarted) return;
  window.__dakiaNativeLocalAclStarted = true;
  const collector = {collector_json};
  const target = {target_json};
  const inventorySpecs = {inventory_json};
  const bounded = value => String(value).slice(0, 1024);
  const encode = value => btoa(String.fromCharCode(...new TextEncoder().encode(JSON.stringify(value))));
  const report = value => {{
    const image = document.createElement('img');
    image.hidden = true;
    image.src = collector + encodeURIComponent(encode(value));
    (window.__dakiaNativeAclReportImages ||= []).push(image);
  }};
  const invoke = window.__TAURI_INTERNALS__?.invoke;
  if (typeof invoke !== 'function') {{ report({{ ipcAvailable: false, probes: [] }}); return; }}
  const specs = [
    ['set_message_read', {{ messageId: target.messageId, read: true, expectedUidValidity: target.expectedUidValidity }}],
    ['save_attachment', {{ messageId: target.messageId, attachmentId: target.attachmentId, expectedUidValidity: target.expectedUidValidity }}],
    ['open_external_url', {{ url: target.openerUrl }}],
    ['send_desktop_notification', {{ notification: {{ title: 'Fictional ACL canary', body: 'This notification must be denied', accountId: '62a9320c-04f5-47c2-b4fa-d4952f0b6121', messageId: target.messageId, rfcMessageId: '<fictional-provider-reader-acceptance@localhost>', threadId: 'fictional-provider-reader-thread@localhost', count: 1, sound: null }} }}],
    ['open_settings_window', {{ accountId: null }}],
    ['plugin:webview|create_webview_window', {{ options: {{ label: 'native-acl-forged', url: '/?view=settings', title: 'Forged settings', width: 640, height: 480 }} }}],
    ['plugin:opener|open_url', {{ url: target.openerUrl, with: null }}],
    ['plugin:notification|notify', {{ options: {{ title: 'Fictional plugin ACL canary', body: 'This notification must be denied' }} }}],
  ];
  (async () => {{
    const probes = [];
    for (const [command, args] of specs) {{
      try {{
        const value = await invoke(command, args);
        probes.push({{ command, outcome: 'resolved', value }});
      }} catch (error) {{
        probes.push({{ command, outcome: 'rejected', error: bounded(error) }});
      }}
    }}
    const inventoryProbes = [];
    for (const {{ command, args }} of inventorySpecs) {{
      try {{
        const value = await invoke(command, args);
        inventoryProbes.push({{ command, outcome: 'resolved', value }});
      }} catch (error) {{
        inventoryProbes.push({{ command, outcome: 'rejected', error: String(error).slice(0, 384) }});
      }}
    }}
    report({{ ipcAvailable: true, href: location.href, probes, inventoryProbes }});
  }})().catch(error => report({{ ipcAvailable: true, fatalError: bounded(error), probes: [] }}));
}})();"#
    );
    let deadline = tokio::time::Instant::now() + RESULT_WAIT;
    loop {
        window.eval(&script)?;
        if let Some(result) = state
            .lock()
            .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
            .local_result
            .clone()
        {
            return Ok(result);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for local unlisted-window Wry IPC probe");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn install(app: &mut tauri::App, state: Arc<AppState>) -> anyhow::Result<()> {
    let Some(paths) = OwnedFixturePaths::from_environment()? else {
        return Ok(());
    };
    let evidence_path = paths.root.join(EVIDENCE_FILENAME);
    if evidence_path.exists() {
        anyhow::bail!("native ACL evidence path already exists in the fresh fixture root");
    }
    let registered_commands = registered_command_inventory()?;
    let permission_commands = permission_command_inventory()?;
    validate_static_probe_scopes(&registered_commands, &permission_commands)?;
    let runtime_commands = RUNTIME_COMMANDS
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let nonce = Uuid::new_v4().simple().to_string();
    let server = ProbeServer::start(nonce.clone())?;
    let probe_url: url::Url = format!("{}/probe?nonce={nonce}", server.origin).parse()?;
    let remote_probe_builder = WebviewWindowBuilder::new(
        app,
        REMOTE_PROBE_WINDOW_LABEL,
        WebviewUrl::External(probe_url.clone()),
    );
    let local_probe_builder = WebviewWindowBuilder::new(
        app,
        LOCAL_PROBE_WINDOW_LABEL,
        WebviewUrl::App("/__dakia_native_acl_probe__".into()),
    );
    #[cfg(target_os = "macos")]
    let (remote_probe_builder, local_probe_builder) =
        match native_mail_safety_data_store_identifier(app.handle())? {
            Some(identifier) => (
                remote_probe_builder.data_store_identifier(identifier),
                local_probe_builder.data_store_identifier(identifier),
            ),
            None => (remote_probe_builder, local_probe_builder),
        };
    let remote_probe_window = remote_probe_builder
        .title("Dakia isolated ACL probe")
        .visible(false)
        .focused(false)
        .build()?;
    let observed_remote_probe_url = remote_probe_window.url()?.to_string();
    if observed_remote_probe_url != probe_url.as_str() {
        anyhow::bail!("native ACL probe webview loaded an unexpected origin");
    }
    let local_probe_window = local_probe_builder
        .initialization_script(
            "if (location.pathname === '/__dakia_native_acl_probe__') { window.stop(); }",
        )
        .title("Dakia local ACL probe")
        .visible(false)
        .focused(false)
        .build()?;
    let observed_local_probe_url = local_probe_window.url()?.to_string();

    let app = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        let executable = executable_evidence().and_then(|value| Ok(serde_json::to_value(value)?));
        let initial_downloads = directory_snapshot(&paths.downloads);
        let account_id = Uuid::parse_str("62a9320c-04f5-47c2-b4fa-d4952f0b6121")
            .expect("fictional fixture account UUID is valid");
        let result: anyhow::Result<Value> = async {
            let initial_downloads = initial_downloads?;
            let (message, attachment_id) = wait_for_target(&state, account_id).await?;
            let expected_uid_validity = message
                .mailbox_uid_validity
                .ok_or_else(|| anyhow::anyhow!("fictional target lacks UIDVALIDITY"))?;
            let opener_url = format!("{}/opener-canary?nonce={nonce}", server.origin);
            let target = ProbeTarget {
                nonce: nonce.clone(),
                account_id: account_id.to_string(),
                message_id: message.id.clone(),
                attachment_id,
                expected_uid_validity,
                recovery_attempt_id: "5d10769d-0d8b-4db1-bc6c-148e8481fe51".into(),
                recovery_key: "d02ab9c5-7640-4599-a35f-772818318a92".into(),
                opener_url,
            };
            server
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
                .target = Some(target.clone());

            let main_control =
                main_bridge_control(&app, &nonce, &server.origin, &server.state).await?;
            let local_result = local_dispatcher_probe(
                &app,
                &target,
                &server.origin,
                &server.state,
                &runtime_commands,
            )
            .await?;
            let deadline = tokio::time::Instant::now() + RESULT_WAIT;
            let remote_result = loop {
                if let Some(result) = server
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
                    .remote_result
                    .clone()
                {
                    break result;
                }
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for remote Wry IPC probe results");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            tokio::time::sleep(Duration::from_millis(500)).await;

            let after_message = state.store.message(&message.id).await?;
            let message_unchanged = after_message.as_ref().is_some_and(|after| {
                after.id == message.id
                    && after.account_id == message.account_id
                    && after.mailbox == message.mailbox
                    && after.uid == message.uid
                    && after.mailbox_uid_validity == message.mailbox_uid_validity
                    && after.is_read == message.is_read
                    && after.is_flagged == message.is_flagged
            });
            let final_downloads = directory_snapshot(&paths.downloads)?;
            let server_state = server
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("probe state poisoned"))?;
            let side_effects = SideEffectEvidence {
                message_unchanged,
                downloads_unchanged: initial_downloads == final_downloads,
                opener_canary_hits: server_state.opener_canary_hits,
                settings_window_absent: app.get_webview_window("settings").is_none(),
                forged_window_absent: app.get_webview_window("native-acl-forged").is_none(),
            };
            let no_side_effects = side_effects.message_unchanged
                && side_effects.downloads_unchanged
                && side_effects.opener_canary_hits == 0
                && side_effects.settings_window_absent
                && side_effects.forged_window_absent;
            let malformed_requests = server_state.malformed_requests;
            drop(server_state);
            let passed = dispatcher_acl_passed(&local_result)
                && inventory_acl_passed(
                    &local_result,
                    &runtime_commands,
                    &permission_commands,
                )
                && remote_isolation_passed(&remote_result)
                && main_control_passed(&main_control)
                && no_side_effects
                && malformed_requests == 0;
            Ok(json!({
                "schemaVersion": 1,
                "status": if passed { "passed" } else { "failed" },
                "feature": "native-mail-safety-fixture",
                "probeFlag": PROBE_FLAG,
                "probeCommands": DENIED_COMMANDS,
                "registeredCommandInventory": registered_commands,
                "runtimeCommandInventory": runtime_commands,
                "staticOnlyCommands": STATIC_ONLY_COMMANDS,
                "executable": executable.as_ref().map_err(|error| anyhow::anyhow!(error.to_string()))?,
                "profile": paths.profile,
                "main": {
                    "label": "main",
                    "url": app.get_webview_window("main").map(|window| window.url().map(|url| url.to_string())).transpose()?,
                    "control": main_control,
                },
                "localProbe": {
                    "label": LOCAL_PROBE_WINDOW_LABEL,
                    "url": observed_local_probe_url,
                    "result": local_result,
                },
                "remoteProbe": {
                    "label": REMOTE_PROBE_WINDOW_LABEL,
                    "url": observed_remote_probe_url,
                    "result": remote_result,
                    "malformedRequests": malformed_requests,
                },
                "target": {
                    "messageId": message.id,
                    "mailbox": message.mailbox,
                    "uid": message.uid,
                    "uidValidity": expected_uid_validity,
                },
                "sideEffects": side_effects,
                "checks": {
                    "mainReadOnlyBridge": main_control_passed(&main_control),
                    "localBridgeReachedAcl": dispatcher_acl_passed(&local_result),
                    "localRegisteredCommandsDenied": inventory_acl_passed(
                        &local_result,
                        &runtime_commands,
                        &permission_commands,
                    ),
                    "staticProbeWindowScopesDenied": true,
                    "remoteOriginIsolated": remote_isolation_passed(&remote_result),
                    "noSideEffects": no_side_effects,
                },
            }))
        }
        .await;

        let evidence = match result {
            Ok(evidence) => evidence,
            Err(error) => json!({
                "schemaVersion": 1,
                "status": "failed",
                "feature": "native-mail-safety-fixture",
                "probeFlag": PROBE_FLAG,
                "probeCommands": DENIED_COMMANDS,
                "registeredCommandInventory": registered_commands,
                "runtimeCommandInventory": runtime_commands,
                "staticOnlyCommands": STATIC_ONLY_COMMANDS,
                "executable": executable.ok(),
                "profile": paths.profile,
                "localProbe": { "label": LOCAL_PROBE_WINDOW_LABEL, "url": observed_local_probe_url },
                "remoteProbe": { "label": REMOTE_PROBE_WINDOW_LABEL, "url": observed_remote_probe_url },
                "error": error.to_string(),
            }),
        };
        if let Err(error) = write_evidence(&evidence_path, &evidence) {
            tracing::error!(error = %error, "could not write native ACL acceptance evidence");
        }
        if let Some(window) = app.get_webview_window(LOCAL_PROBE_WINDOW_LABEL) {
            let _ = window.close();
        }
        if let Some(window) = app.get_webview_window(REMOTE_PROBE_WINDOW_LABEL) {
            let _ = window.close();
        }
        server.stop();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_complete_and_private_before_the_final_path_becomes_visible() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary evidence directory");
        let evidence_path = directory.path().join(EVIDENCE_FILENAME);
        let evidence = json!({
            "schemaVersion": 1,
            "status": "passed",
            "payload": "x".repeat(256 * 1024),
        });

        write_evidence_before_publish(&evidence_path, &evidence, |temporary_path| {
            assert!(!evidence_path.exists());
            assert_eq!(
                std::fs::metadata(temporary_path)?.permissions().mode() & 0o777,
                0o600
            );
            let temporary: Value = serde_json::from_slice(&std::fs::read(temporary_path)?)?;
            assert_eq!(temporary, evidence);
            Ok(())
        })
        .expect("atomically write evidence");

        let published: Value =
            serde_json::from_slice(&std::fs::read(&evidence_path).unwrap()).unwrap();
        assert_eq!(published, evidence);
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "temporary evidence file must be removed after publication"
        );
    }

    #[test]
    fn evidence_publication_never_replaces_an_existing_result() {
        let directory = tempfile::tempdir().expect("temporary evidence directory");
        let evidence_path = directory.path().join(EVIDENCE_FILENAME);
        let first = json!({ "schemaVersion": 1, "status": "failed", "attempt": 1 });
        let second = json!({ "schemaVersion": 1, "status": "passed", "attempt": 2 });

        write_evidence(&evidence_path, &first).expect("write first evidence");
        assert!(write_evidence(&evidence_path, &second).is_err());
        let published: Value =
            serde_json::from_slice(&std::fs::read(&evidence_path).unwrap()).unwrap();
        assert_eq!(published, first);
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "failed publication must clean its temporary file"
        );
    }

    #[test]
    fn image_collector_bypasses_idle_preconnection_without_false_malformed_request() {
        let nonce = "native-acl-collector-test".to_owned();
        let server = ProbeServer::start(nonce.clone()).expect("start probe server");
        let address: SocketAddr = server
            .origin
            .strip_prefix("http://")
            .expect("loopback origin")
            .parse()
            .expect("loopback address");
        let _idle_connection = TcpStream::connect(address).expect("open idle preconnection");

        let expected = json!({ "ipcAvailable": true, "outcome": "resolved", "value": [] });
        let encoded = STANDARD.encode(serde_json::to_vec(&expected).expect("serialize result"));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("nonce", &nonce)
            .append_pair("data", &encoded)
            .finish();
        let mut request = TcpStream::connect(address).expect("connect real collector request");
        request
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set response timeout");
        write!(
            request,
            "GET /main-result?{query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            address
        )
        .expect("send collector request");
        let mut response = String::new();
        request
            .read_to_string(&mut response)
            .expect("read collector response while idle connection remains open");
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert_eq!(
            server.state.lock().expect("probe state").main_result,
            Some(expected)
        );

        thread::sleep(Duration::from_millis(2_100));
        assert_eq!(
            server
                .state
                .lock()
                .expect("probe state after idle timeout")
                .malformed_requests,
            0
        );
        server.stop();
    }

    #[test]
    fn exhaustive_inventory_is_source_coupled_and_rejects_post_acl_outcomes() {
        let registered = registered_command_inventory().expect("registered command inventory");
        let permissions = permission_command_inventory().expect("permission command inventory");
        assert!(registered.len() >= 59);
        assert!(registered.iter().any(|command| command == "send_message"));
        assert!(registered.iter().any(|command| command == "remove_account"));
        assert!(registered.iter().any(|command| command == "set_ai_api_key"));
        assert!(permissions.contains("send_message"));
        assert!(permissions.contains("quit_startup_recovery"));
        assert!(!permissions.contains("set_ai_api_key"));
        for command in [
            "list_send_recoveries",
            "open_send_recovery_window",
            "read_send_recovery_seed",
            "acknowledge_send_recovery_seed",
        ] {
            assert!(registered.iter().any(|registered| registered == command));
            assert!(RUNTIME_COMMANDS.contains(&command));
            assert!(permissions.contains(command));
        }
        validate_static_probe_scopes(&registered, &permissions)
            .expect("static capability scopes and command partition");
        let capability_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut capability_files = std::fs::read_dir(capability_directory)
            .expect("read capability directory")
            .map(|entry| {
                entry
                    .expect("capability entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        capability_files.sort();
        assert_eq!(
            capability_files,
            CAPABILITY_FILES
                .iter()
                .map(|(filename, _)| (*filename).to_owned())
                .collect::<Vec<_>>()
        );

        let capability = |filename: &str| {
            CAPABILITY_FILES
                .iter()
                .find_map(|(candidate, source)| (*candidate == filename).then_some(*source))
                .and_then(|source| serde_json::from_str::<Value>(source).ok())
                .expect("declared capability must be valid JSON")
        };
        for filename in ["default.json", "settings.json"] {
            let base = capability(filename);
            let permissions = base["permissions"]
                .as_array()
                .expect("capability permissions");
            assert!(permissions
                .iter()
                .all(|permission| permission != "notification:default"));
        }
        let macos = capability("notification-permission-macos.json");
        assert_eq!(macos["windows"], json!(["main", "settings"]));
        assert_eq!(macos["platforms"], json!(["macOS"]));
        assert_eq!(
            macos["permissions"],
            json!([
                "notification:allow-is-permission-granted",
                "notification:allow-request-permission"
            ])
        );
        let non_macos = capability("notification-non-macos.json");
        assert_eq!(non_macos["windows"], json!(["main", "settings"]));
        assert_eq!(non_macos["platforms"], json!(["linux", "windows"]));
        assert_eq!(non_macos["permissions"], json!(["notification:default"]));
        let fatal_recovery = capability("fatal-recovery.json");
        assert_eq!(fatal_recovery["windows"], json!(["fatal-recovery"]));
        assert_eq!(
            fatal_recovery["permissions"],
            json!(["allow-fatal-recovery"])
        );
        assert!(fatal_recovery["permissions"]
            .as_array()
            .expect("fatal recovery permissions")
            .iter()
            .all(|permission| permission != "core:default"));
        let fatal_permission = include_str!("../permissions/mail-commands.toml")
            .split("[[permission]]")
            .find(|section| section.contains("identifier = \"allow-fatal-recovery\""))
            .expect("fatal recovery permission declaration");
        assert!(fatal_permission.contains("\"quit_startup_recovery\""));
        assert!(!fatal_permission.contains("\"send_message\""));

        let settings_permission = include_str!("../permissions/mail-commands.toml")
            .split("[[permission]]")
            .find(|section| section.contains("identifier = \"allow-settings-mail\""))
            .expect("settings permission declaration");
        assert!(settings_permission.contains("\"send_desktop_notification\","));
        let main_permission = include_str!("../permissions/mail-commands.toml")
            .split("[[permission]]")
            .find(|section| section.contains("identifier = \"allow-main-mail\""))
            .expect("main permission declaration");
        assert!(main_permission.contains("\"list_send_recoveries\","));
        assert!(main_permission.contains("\"open_send_recovery_window\","));
        assert!(!main_permission.contains("\"read_send_recovery_seed\","));
        assert!(!main_permission.contains("\"acknowledge_send_recovery_seed\","));
        let compose_permission = include_str!("../permissions/mail-commands.toml")
            .split("[[permission]]")
            .find(|section| section.contains("identifier = \"allow-compose-mail\""))
            .expect("compose permission declaration");
        assert!(compose_permission.contains("\"read_send_recovery_seed\","));
        assert!(compose_permission.contains("\"acknowledge_send_recovery_seed\","));
        assert!(!compose_permission.contains("\"list_send_recoveries\","));
        assert!(!compose_permission.contains("\"open_send_recovery_window\","));

        let runtime_commands = RUNTIME_COMMANDS
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(READ_ONLY_NO_ARGUMENT_COMMANDS
            .iter()
            .all(|command| RUNTIME_COMMANDS.contains(command)));

        let mut probes = runtime_commands
            .iter()
            .map(|command| {
                let error = if permissions.contains(command) {
                    format!(
                        "{command} not allowed on window \"native-acl-local-probe\", webview \"native-acl-local-probe\", URL: local"
                    )
                } else {
                    format!("{command} not allowed. Command not found")
                };
                json!({ "command": command, "outcome": "rejected", "error": error })
            })
            .collect::<Vec<_>>();
        assert!(inventory_acl_passed(
            &json!({ "inventoryProbes": probes }),
            &runtime_commands,
            &permissions,
        ));

        let send_index = runtime_commands
            .iter()
            .position(|command| command == "send_message")
            .expect("send command index");
        probes[send_index] = json!({
            "command": "send_message",
            "outcome": "rejected",
            "error": "missing field `message`",
        });
        assert!(!inventory_acl_passed(
            &json!({ "inventoryProbes": probes }),
            &runtime_commands,
            &permissions,
        ));

        let target = ProbeTarget {
            nonce: "native-acl-recovery-test".into(),
            account_id: "62a9320c-04f5-47c2-b4fa-d4952f0b6121".into(),
            message_id: "fixture-message".into(),
            attachment_id: "fixture-attachment".into(),
            expected_uid_validity: 17,
            recovery_attempt_id: "5d10769d-0d8b-4db1-bc6c-148e8481fe51".into(),
            recovery_key: "d02ab9c5-7640-4599-a35f-772818318a92".into(),
            opener_url: "http://127.0.0.1/fixture".into(),
        };
        let runtime_commands = RUNTIME_COMMANDS
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let specs = inventory_probe_specs(&runtime_commands, &target);
        let args_for = |command| {
            specs
                .iter()
                .find(|spec| spec["command"] == command)
                .and_then(|spec| spec.get("args"))
                .cloned()
                .expect("recovery runtime command must have one probe spec")
        };
        assert_eq!(
            args_for("list_send_recoveries"),
            json!({ "accountIds": [&target.account_id], "cursor": null })
        );
        assert_eq!(
            args_for("open_send_recovery_window"),
            json!({
                "accountId": &target.account_id,
                "attemptId": &target.recovery_attempt_id,
            })
        );
        for command in ["read_send_recovery_seed", "acknowledge_send_recovery_seed"] {
            assert_eq!(
                args_for(command),
                json!({ "recoveryKey": &target.recovery_key })
            );
        }

        let permissions = permission_command_inventory().expect("permission command inventory");
        let mut probes = runtime_commands
            .iter()
            .map(|command| {
                json!({
                    "command": command,
                    "outcome": "rejected",
                    "error": format!(
                        "{command} not allowed on window \"native-acl-local-probe\", webview \"native-acl-local-probe\", URL: local"
                    ),
                })
            })
            .collect::<Vec<_>>();
        assert!(inventory_acl_passed(
            &json!({ "inventoryProbes": probes }),
            &runtime_commands,
            &permissions,
        ));
        for command in [
            "list_send_recoveries",
            "open_send_recovery_window",
            "read_send_recovery_seed",
            "acknowledge_send_recovery_seed",
        ] {
            let index = runtime_commands
                .iter()
                .position(|candidate| candidate == command)
                .expect("recovery command in runtime inventory");
            let original = probes[index].clone();
            probes[index] = json!({
                "command": command,
                "outcome": "rejected",
                "error": "saved message is no longer available",
            });
            assert!(
                !inventory_acl_passed(
                    &json!({ "inventoryProbes": probes }),
                    &runtime_commands,
                    &permissions,
                ),
                "a post-handler recovery error must not count as an ACL denial for {command}"
            );
            probes[index] = original;
        }
    }

    #[test]
    fn acl_result_requires_real_ipc_and_acl_rejections_for_every_probe() {
        let probes = [
            "set_message_read",
            "save_attachment",
            "open_external_url",
            "send_desktop_notification",
            "open_settings_window",
            "plugin:webview|create_webview_window",
            "plugin:opener|open_url",
            "plugin:notification|notify",
        ]
        .map(|command| {
            json!({
                "command": command,
                "outcome": "rejected",
                "error": format!("Command {command} not allowed by ACL"),
            })
        });
        assert!(dispatcher_acl_passed(
            &json!({ "ipcAvailable": true, "probes": probes })
        ));
        assert!(!dispatcher_acl_passed(
            &json!({ "ipcAvailable": false, "probes": probes })
        ));
        assert!(remote_isolation_passed(
            &json!({ "ipcAvailable": false, "probes": [] })
        ));
        let mut invalid = probes;
        invalid[0] = json!({ "command": "set_message_read", "outcome": "rejected", "error": "Message not found" });
        assert!(!dispatcher_acl_passed(
            &json!({ "ipcAvailable": true, "probes": invalid })
        ));
    }

    #[test]
    fn acl_result_rejects_unknown_commands_and_plugins() {
        assert!(!registered_acl_denial(
            "set_message_read",
            "set_message_read not allowed. Command not found"
        ));
        assert!(!registered_acl_denial(
            "plugin:opener|open_url",
            "opener.open_url not allowed. Plugin not found"
        ));
        assert!(!registered_acl_denial(
            "set_message_read",
            "some_other_command not allowed on window \"native-acl-local-probe\", webview \"native-acl-local-probe\", URL: local"
        ));
        assert!(!registered_acl_denial(
            "set_message_read",
            "set_message_read not allowed"
        ));
    }

    #[test]
    fn acl_result_accepts_registered_debug_context_denials() {
        assert!(registered_acl_denial(
            "set_message_read",
            "set_message_read not allowed on window \"native-acl-local-probe\", webview \"native-acl-local-probe\", URL: local\n\nallowed on: [windows: \"main\", URL: local]\n\nreferenced by: capability: main, permission: allow-set-message-read"
        ));
        assert!(registered_acl_denial(
            "plugin:opener|open_url",
            "opener.open_url not allowed on origin [remote:http://127.0.0.1:1234/]. Please create a capability that has this origin on the context field."
        ));
        assert!(registered_acl_denial(
            "plugin:notification|notify",
            "notification.notify not allowed. Permissions associated with this command: notification:allow-notify"
        ));
    }
}
