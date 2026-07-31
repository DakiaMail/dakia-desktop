import assert from "node:assert/strict";
import test from "node:test";
import {
  assertValidInventory,
  extractFrontendEmitters,
  extractFrontendInvokeRequests,
  extractFrontendInvokes,
  extractFrontendListeners,
  extractGenerateHandlerCommands,
  extractRustCommandParameters,
  extractRustNativeEmits,
  inventoryRepository,
  readContract,
  validateInventory,
} from "./tauri-contract-inventory.mjs";

test("parses production invokes while ignoring comments and quoted lookalikes", () => {
  const source = `
    // invoke("commented_out")
    const note = 'invoke("quoted_out")';
    invoke<Result<{ payload: Array<string> }>>("real_command", { payload: [] });
    invoke("second_command");
  `;
  assert.deepEqual(extractFrontendInvokes(source), [
    "real_command",
    "second_command",
  ]);
});

test("rejects dynamic frontend contract names instead of silently omitting them", () => {
  assert.throws(
    () => extractFrontendInvokes("invoke(`command-${suffix}`)"),
    /must use a literal contract name/,
  );
  assert.throws(
    () => extractFrontendListeners("listen(eventName, handler)"),
    /must use a literal contract name/,
  );
});

test("captures no-argument and static top-level frontend invoke request keys", () => {
  const source = `
    invoke("no_arguments");
    invoke("hydrate_message", { messageId });
    invoke("sync_account", {
      accountId,
      limit: full ? 250 : 50,
      full,
      onProgress: channel,
    });
  `;
  assert.deepEqual(extractFrontendInvokeRequests(source), [
    { command: "no_arguments", keys: [] },
    { command: "hydrate_message", keys: ["messageId"] },
    {
      command: "sync_account",
      keys: ["accountId", "full", "limit", "onProgress"],
    },
  ]);
});

test("rejects opaque, spread, and computed frontend invoke request keys", () => {
  assert.throws(
    () => extractFrontendInvokeRequests('invoke("command", request)'),
    /must be a static object literal/,
  );
  assert.throws(
    () => extractFrontendInvokeRequests('invoke("command", { ...request })'),
    /must not use spread properties/,
  );
  assert.throws(
    () => extractFrontendInvokeRequests('invoke("command", { [key]: value })'),
    /must use static top-level keys/,
  );
});

test("maps Rust command parameters to frontend keys and excludes injected parameters", () => {
  const source = `
    #[tauri::command]
    async fn hydrate_message(
      app: tauri::AppHandle,
      state: State<'_, Arc<AppState>>,
      message_id: String,
    ) -> Result<(), String> { Ok(()) }

    #[tauri::command]
    async fn sync_account(
      account_id: Uuid,
      on_progress: Channel<SyncProgress>,
      window: tauri::Window,
    ) -> Result<(), String> { Ok(()) }
  `;
  assert.deepEqual(extractRustCommandParameters(source), {
    hydrate_message: ["messageId"],
    sync_account: ["accountId", "onProgress"],
  });
});

test("parses Rust handler macros and native event constants", () => {
  const source = `
    const RECEIPT: &str = "dakia://receipt";
    // app.emit("commented", value);
    builder.invoke_handler(tauri::generate_handler![
      first_command,
      second_command,
    ]);
    app.emit(RECEIPT, value)?;
    app.emit("mail-arrived", value)?;
  `;
  assert.deepEqual(extractGenerateHandlerCommands(source), [
    "first_command",
    "second_command",
  ]);
  assert.deepEqual(extractRustNativeEmits(source), [
    "dakia://receipt",
    "mail-arrived",
  ]);
});

test("does not confuse Rust lifetimes with quoted event literals", () => {
  const source = `
    async fn command(state: State<'_, AppState>) {}
    app.emit("lifetime-safe", payload)?;
  `;
  assert.deepEqual(extractRustNativeEmits(source), ["lifetime-safe"]);
});

test("parses frontend event listener and cross-window emitter argument positions", () => {
  const source = `
    listen<Event>("native-event", handler);
    getCurrentWebview().listen("webview-event", handler);
    emitTo("main", "frontend-event", payload);
  `;
  assert.deepEqual(extractFrontendListeners(source), [
    "native-event",
    "webview-event",
  ]);
  assert.deepEqual(extractFrontendEmitters(source), ["frontend-event"]);
});

test("reports command, origin, and event drift with demo-only exclusions explicit", () => {
  const inventory = {
    frontendInvokes: ["registered", "demo_command"],
    frontendInvokeRequests: [
      { command: "registered", keys: [] },
      { command: "demo_command", keys: [] },
    ],
    rustHandlers: ["registered", "orphaned"],
    rustCommandParameters: { registered: [], orphaned: [] },
    frontendListeners: ["native-event", "frontend-event", "unclassified"],
    frontendEmitters: ["frontend-event", "wrong-event"],
    rustNativeEmits: ["native-event", "orphaned-event"],
  };
  const contract = {
    schemaVersion: 1,
    demoOnlyInvokes: ["demo_command"],
    eventOrigins: {
      "native-event": "native",
      "frontend-event": "frontend",
    },
  };
  const errors = validateInventory(inventory, contract);
  assert.match(
    errors.join("\n"),
    /frontend invokes vs Rust handlers missing: orphaned/,
  );
  assert.match(
    errors.join("\n"),
    /frontend event classifications unexpected: unclassified/,
  );
  assert.match(
    errors.join("\n"),
    /native emitted events vs native listeners unexpected: orphaned-event/,
  );
  assert.match(
    errors.join("\n"),
    /frontend emitted events vs frontend listeners unexpected: wrong-event/,
  );
  assert.throws(
    () => assertValidInventory(inventory, contract),
    /Tauri contract drift/,
  );
});

test("reports Rust handler parameter renames that command-name inventory misses", () => {
  const rustCommandParameters = extractRustCommandParameters(`
    #[tauri::command]
    async fn hydrate_message(message: String) -> Result<(), String> { Ok(()) }

    #[tauri::command]
    async fn sync_account(account_id: Uuid, progress: Channel<SyncProgress>) -> Result<(), String> { Ok(()) }
  `);
  const inventory = {
    frontendInvokes: ["hydrate_message", "sync_account"],
    frontendInvokeRequests: [
      { command: "hydrate_message", keys: ["messageId"] },
      {
        command: "sync_account",
        keys: ["accountId", "onProgress"],
      },
    ],
    rustHandlers: ["hydrate_message", "sync_account"],
    rustCommandParameters,
    frontendListeners: [],
    frontendEmitters: [],
    rustNativeEmits: [],
  };
  const contract = {
    schemaVersion: 1,
    demoOnlyInvokes: [],
    eventOrigins: {},
  };
  const errors = validateInventory(inventory, contract).join("\n");
  assert.match(
    errors,
    /frontend invoke request keys for hydrate_message missing: message/,
  );
  assert.match(
    errors,
    /frontend invoke request keys for hydrate_message unexpected: messageId/,
  );
  assert.match(
    errors,
    /frontend invoke request keys for sync_account missing: progress/,
  );
  assert.match(
    errors,
    /frontend invoke request keys for sync_account unexpected: onProgress/,
  );
});

test("the checked-in production Tauri inventory is synchronized", async () => {
  const [inventory, contract] = await Promise.all([
    inventoryRepository(),
    readContract(),
  ]);
  assert.deepEqual(validateInventory(inventory, contract), []);
});
