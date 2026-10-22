import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, realpathSync, statSync } from "node:fs";
import { relative, resolve } from "node:path";

export const CATALOGUE_PATH = "testdata/mail-safety-catalogue.json";

const AREAS = new Set("ABCDEFGHIJKL".split(""));
const PRIORITIES = new Set(["P0", "P1", "P2"]);
const STATUSES = new Set([
  "implemented",
  "gap",
  "manual-required",
  "unsupported",
]);
const LANES = new Set([
  "pull-request",
  "manual",
  "provider",
  "native",
  "release",
]);
const STANDARD_STATUSES = new Set([
  "tested",
  "partial",
  "not-claimed",
  "not-applicable",
]);
const INPUT_KINDS = new Set([
  "source-test",
  "fixture",
  "manual-fixture",
  "not-captured",
]);
const OUTCOME_STATUSES = new Set(["expected", "not-applicable", "not-defined"]);

// Keep the catalogue's readable production-entrypoint labels tied to concrete
// declarations.  This deliberately rejects plausible but nonexistent names
// such as MailService::full_snapshot instead of accepting a prose label.
const ENTRYPOINT_REFERENCES = {
  AiService: ["crates/dakia-core/src/ai.rs", "AiService"],
  App: ["apps/desktop/src/App.tsx", "App"],
  "Attachment::is_potentially_unsafe": [
    "crates/dakia-core/src/storage.rs",
    "is_potentially_unsafe",
  ],
  ComposeApp: ["apps/desktop/src/ComposeApp.tsx", "ComposeApp"],
  "Dakia.app": ["apps/desktop/src-tauri/src/lib.rs", "run"],
  HtmlMessage: ["apps/desktop/src/components/HtmlMessage.tsx", "HtmlMessage"],
  ImapClient: ["crates/dakia-core/src/mail.rs", "ImapClient"],
  "ImapClient::connect_with_endpoint": [
    "crates/dakia-core/src/mail.rs",
    "connect_with_endpoint",
  ],
  MailService: ["crates/dakia-core/src/mail.rs", "MailService"],
  "MailService::fetch_message": [
    "crates/dakia-core/src/mail.rs",
    "fetch_message",
  ],
  "MailService::fetch_new_inbox_headers": [
    "crates/dakia-core/src/mail.rs",
    "fetch_new_inbox_headers",
  ],
  "MailService::fetch_attachment_at_epoch": [
    "crates/dakia-core/src/mail.rs",
    "fetch_attachment_at_epoch",
  ],
  "MailService::apply_action_at_epoch": [
    "crates/dakia-core/src/mail.rs",
    "apply_action_at_epoch",
  ],
  "MailService::imap_auth_probe": [
    "crates/dakia-core/src/mail.rs",
    "imap_auth_probe",
  ],
  "MailService::realtime_inbox_cycle": [
    "crates/dakia-core/src/mail.rs",
    "realtime_inbox_cycle",
  ],
  "MailService::refresh_inbox_with_progress": [
    "crates/dakia-core/src/mail.rs",
    "refresh_inbox_with_progress",
  ],
  "MailService::send": ["crates/dakia-core/src/mail.rs", "send"],
  "MailService::send_with_endpoints": [
    "crates/dakia-core/src/mail.rs",
    "send_with_endpoints",
  ],
  "MailService::send_with_smtp_endpoint": [
    "crates/dakia-core/src/mail.rs",
    "send_with_smtp_endpoint",
  ],
  "MailService::set_password": [
    "crates/dakia-core/src/mail.rs",
    "set_password",
  ],
  "MailService::smtp_auth_probe": [
    "crates/dakia-core/src/mail.rs",
    "smtp_auth_probe",
  ],
  "MailService::unsubscribe": ["crates/dakia-core/src/mail.rs", "unsubscribe"],
  MimeBudget: ["crates/dakia-core/src/mime_budget.rs", "MimeBudgetError"],
  Reader: ["apps/desktop/src/components/Reader.tsx", "Reader"],
  RealtimeSyncManager: [
    "apps/desktop/src-tauri/src/realtime.rs",
    "RealtimeSyncManager",
  ],
  "RealtimeSyncManager::reconcile": [
    "apps/desktop/src-tauri/src/realtime.rs",
    "reconcile",
  ],
  "RealtimeSyncManager::run_watcher": [
    "apps/desktop/src-tauri/src/realtime.rs",
    "run_watcher",
  ],
  Store: ["crates/dakia-core/src/storage.rs", "Store"],
  "Store::cache_message_content": [
    "crates/dakia-core/src/storage.rs",
    "cache_message_content",
  ],
  "Store::claim_send_attempt": [
    "crates/dakia-core/src/send_journal.rs",
    "claim_send_attempt",
  ],
  "Store::delete_account": [
    "crates/dakia-core/src/storage.rs",
    "delete_account",
  ],
  "CredentialStore::secret": ["crates/dakia-core/src/mail.rs", "secret"],
  "Store::secret": ["crates/dakia-core/src/storage.rs", "secret"],
  "Store::migrate": ["crates/dakia-core/src/storage.rs", "migrate"],
  "Store::open": ["crates/dakia-core/src/storage.rs", "open"],
  "Store::remove_account": [
    "crates/dakia-core/src/storage.rs",
    "delete_account",
  ],
  "Store::save_message_content": [
    "crates/dakia-core/src/storage.rs",
    "cache_message_content",
  ],
  "Store::upsert_messages": [
    "crates/dakia-core/src/storage.rs",
    "upsert_messages",
  ],
  "Store::save_synced_messages": [
    "crates/dakia-core/src/storage.rs",
    "save_synced_messages",
  ],
  TauriRealtimeEventSink: [
    "apps/desktop/src-tauri/src/realtime.rs",
    "TauriRealtimeEventSink",
  ],
  analytics: ["apps/desktop/src/analytics.ts", "analyticsProvider"],
  ai_draft: ["apps/desktop/src-tauri/src/lib.rs", "ai_draft"],
  ai_summarize: ["apps/desktop/src-tauri/src/lib.rs", "ai_summarize"],
  "api.send": ["apps/desktop/src/api.ts", "api"],
  api: ["apps/desktop/src/api.ts", "api"],
  apply_mailbox_action: [
    "apps/desktop/src-tauri/src/lib.rs",
    "apply_mailbox_action",
  ],
  attachment_presentation: [
    "crates/dakia-core/src/mail.rs",
    "attachment_presentation",
  ],
  benchmark: ["crates/dakia-core/src/mail.rs", "refresh_inbox_with_progress"],
  buildEmailDocument: [
    "apps/desktop/src/components/HtmlMessage.tsx",
    "buildEmailDocument",
  ],
  build_message: ["crates/dakia-core/src/mail.rs", "build_compose_message"],
  "cargo-fuzz": ["fuzz/Cargo.toml", "mime_budget"],
  classification: [
    "crates/dakia-core/src/classification.rs",
    "LocalEmailClassifier",
  ],
  composeSeedFromMailto: [
    "apps/desktop/src/mailto.ts",
    "composeSeedFromMailto",
  ],
  current_message_at_epoch: [
    "apps/desktop/src-tauri/src/lib.rs",
    "current_message_at_epoch",
  ],
  "dakia-cli": ["crates/dakia-cli/src/main.rs", "main"],
  download_attachment: ["crates/dakia-cli/src/main.rs", "download_attachment"],
  external_server_roundtrip: [
    "crates/dakia-core/src/independent_server_tests.rs",
    "external_server_roundtrip",
  ],
  extract_zip_archive: [
    "vendor/tauri-plugin-updater/src/updater.rs",
    "extract_zip_archive",
  ],
  external_server_greenmail_roundtrip: [
    "crates/dakia-core/src/independent_server_tests.rs",
    "external_server_greenmail_roundtrip",
  ],
  extract_attachments: ["crates/dakia-core/src/mail.rs", "extract_attachments"],
  extract_unsubscribe: ["crates/dakia-core/src/mail.rs", "extract_unsubscribe"],
  dkim_results_cover_unsubscribe_headers: [
    "crates/dakia-core/src/mail.rs",
    "dkim_results_cover_unsubscribe_headers",
  ],
  fetch_message_content: [
    "apps/desktop/src-tauri/src/lib.rs",
    "load_message_content",
  ],
  fetch_selective_message: [
    "crates/dakia-core/src/mail.rs",
    "fetch_selective_message",
  ],
  invoke_handler: ["apps/desktop/src-tauri/src/lib.rs", "generate_handler"],
  is_potentially_unsafe: [
    "crates/dakia-core/src/mail.rs",
    "is_potentially_unsafe",
  ],
  "offline-translation": [
    "apps/desktop/src-tauri/src/translation.rs",
    "detect_language",
  ],
  open_external_url: ["apps/desktop/src-tauri/src/lib.rs", "open_external_url"],
  parse_message: ["crates/dakia-core/src/mail.rs", "parse_message"],
  parse_catalog_message: [
    "crates/dakia-core/src/mail.rs",
    "parse_catalog_message",
  ],
  "provider-smoke-contract": [
    "scripts/provider-smoke-contract.mjs",
    "validateProviderSmokeContract",
  ],
  require_permanent_delete_locator: [
    "apps/desktop/src-tauri/src/lib.rs",
    "require_permanent_delete_locator",
  ],
  retry_delay: ["apps/desktop/src-tauri/src/realtime.rs", "retry_delay"],
  run_bounded_cancellable_ordered: [
    "apps/desktop/src-tauri/src/realtime.rs",
    "run_bounded_cancellable_ordered",
  ],
  run_hydration_worker: [
    "apps/desktop/src-tauri/src/realtime.rs",
    "run_hydration_worker",
  ],
  pending_hydration_event: [
    "apps/desktop/src-tauri/src/realtime.rs",
    "pending_hydration_event",
  ],
  safe_attachment_filename: [
    "crates/dakia-core/src/mail.rs",
    "safe_attachment_filename",
  ],
  save_attachment: ["apps/desktop/src-tauri/src/lib.rs", "save_attachment"],
  save_all_attachments: [
    "apps/desktop/src-tauri/src/lib.rs",
    "save_all_attachments",
  ],
  save_private_download: [
    "apps/desktop/src-tauri/src/lib.rs",
    "save_private_download",
  ],
  "Store::search": ["crates/dakia-core/src/storage.rs", "search"],
  search_messages: ["crates/dakia-core/src/storage.rs", "search"],
  send_smtp_raw: ["crates/dakia-core/src/mail.rs", "send_smtp_raw"],
  submit_send_attempt: [
    "apps/desktop/src-tauri/src/lib.rs",
    "submit_send_attempt",
  ],
  "tauri-contract-inventory": [
    "scripts/tauri-contract-inventory.mjs",
    "inventoryRepository",
  ],
  "tauri.conf": ["apps/desktop/src-tauri/tauri.conf.json", "productName"],
  tauri_contracts_tests: [
    "apps/desktop/src-tauri/src/lib.rs",
    "tauri_contracts_tests",
  ],
  thread_messages: [
    "crates/dakia-core/src/storage.rs",
    "conversation_for_target",
  ],
  "validate-realistic-fixtures": [
    "scripts/validate-realistic-fixtures.mjs",
    "validateFixtureCorpus",
  ],
  "verify-updater-signature": ["apps/desktop/src-tauri/src/lib.rs", "Builder"],
};

function add(errors, message) {
  errors.push(message);
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function strings(value) {
  return (
    Array.isArray(value) && value.every((item) => typeof item === "string")
  );
}

function nonEmptyString(value) {
  return typeof value === "string" && value.trim().length > 0;
}

function validateInput(input, label, errors, root) {
  if (
    !isObject(input) ||
    !INPUT_KINDS.has(input.kind) ||
    !nonEmptyString(input.id)
  ) {
    add(
      errors,
      `${label} needs a traceable fixture or source input identifier.`,
    );
    return;
  }
  if (input.kind === "not-captured" && !nonEmptyString(input.reason)) {
    add(errors, `${label} not-captured input needs a reason.`);
  }
  if (input.kind === "fixture" && !existsSync(resolve(root, input.id))) {
    add(errors, `${label} fixture input does not exist: ${input.id}.`);
  }
  if (input.kind === "source-test") {
    const separator = input.id.lastIndexOf("#");
    if (separator <= 0 || separator === input.id.length - 1) {
      add(
        errors,
        `${label} source-test input must use path#exact-test-symbol.`,
      );
    } else {
      const problem = sourceContains(root, {
        path: input.id.slice(0, separator),
        symbol: input.id.slice(separator + 1),
      });
      if (problem) add(errors, `${label} source-test input ${problem}`);
    }
  }
}

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function reportSourceDigest(report) {
  const sourceTree = report?.sourceTree;
  if (!isObject(sourceTree)) return null;
  const digests = [];
  if (nonEmptyString(sourceTree.digest)) digests.push(sourceTree.digest);
  if (
    nonEmptyString(sourceTree.before) &&
    sourceTree.before === sourceTree.after
  ) {
    digests.push(sourceTree.before);
  } else if (
    sourceTree.before !== undefined ||
    sourceTree.after !== undefined
  ) {
    return null;
  }
  const after = report?.sourceTreeAfter;
  if (after !== undefined) {
    if (!isObject(after) || !nonEmptyString(after.digest)) return null;
    digests.push(after.digest);
  }
  return digests.length > 0 && new Set(digests).size === 1 ? digests[0] : null;
}

export function isOwnedRegularReport(root, report) {
  if (!/^docs\/mail-safety-results\/[^/]+\.json$/.test(report)) return false;
  const reportsRoot = resolve(root, "docs/mail-safety-results");
  const candidate = resolve(root, report);
  if (!existsSync(candidate)) return false;
  let canonicalRoot;
  let canonicalCandidate;
  try {
    canonicalRoot = realpathSync(reportsRoot);
    canonicalCandidate = realpathSync(candidate);
  } catch {
    return false;
  }
  const pathWithinReports = relative(canonicalRoot, canonicalCandidate);
  if (
    pathWithinReports === "" ||
    pathWithinReports.startsWith("..") ||
    pathWithinReports.includes("../")
  ) {
    return false;
  }
  return statSync(canonicalCandidate).isFile();
}

const EXECUTION_CASES = Object.freeze({
  "native-attachment-quarantine": {
    scenarios: new Set(["MS-G-004"]),
    proves(report) {
      const safe = report?.attachments?.safeSave;
      const leaf = report?.attachments?.leafSymlink;
      return (
        nonEmptyString(safe?.quarantine) &&
        safe?.mode === "0600" &&
        nonEmptyString(leaf?.quarantine) &&
        Number.parseInt(String(leaf?.savedMode), 8) === 0o600
      );
    },
  },
  "dovecot-roundtrip": {
    scenarios: new Set(["MS-L-015"]),
    proves(report) {
      const dovecot = report?.independentServers?.dovecot;
      return (
        Number.isInteger(dovecot?.selectiveCommandCount) &&
        dovecot.selectiveCommandCount > 0 &&
        dovecot.fullRawFallbackObserved === false &&
        dovecot.mutatingStoreObserved === false
      );
    },
  },
  "mime-attachment-fuzz": {
    scenarios: new Set(["MS-L-016"]),
    proves(report) {
      const fuzz = report?.fuzz;
      return (
        Number.isInteger(fuzz?.mimeBudgetExecutions) &&
        fuzz.mimeBudgetExecutions > 0 &&
        Number.isInteger(fuzz?.attachmentMetadataExecutions) &&
        fuzz.attachmentMetadataExecutions > 0 &&
        fuzz.crashArtifacts === 0
      );
    },
  },
  "greenmail-roundtrip": {
    scenarios: new Set(["MS-L-017"]),
    proves(report) {
      return (
        report?.independentServers?.greenmail?.publicRootTlsRejected === true &&
        report?.independentServers?.greenmail?.trustedSmtpImapRoundtrip === true
      );
    },
  },
});

function validateExecutionEvidence(evidence, scenario, label, errors, root) {
  if (evidence === undefined) return;
  if (
    !isObject(evidence) ||
    !nonEmptyString(evidence.report) ||
    !/^[a-f0-9]{64}$/.test(evidence.reportSha256 ?? "") ||
    !/^[a-f0-9]{64}$/.test(evidence.sourceDigest ?? "") ||
    !nonEmptyString(evidence.case)
  ) {
    add(
      errors,
      `${label} executionEvidence needs an owned JSON report, SHA-256, source digest, and bounded case.`,
    );
    return;
  }
  if (!isOwnedRegularReport(root, evidence.report)) {
    add(
      errors,
      `${label} executionEvidence report is not an owned regular report: ${evidence.report}.`,
    );
    return;
  }
  const path = resolve(root, evidence.report);
  if (sha256File(path) !== evidence.reportSha256) {
    add(
      errors,
      `${label} executionEvidence report SHA-256 does not match: ${evidence.report}.`,
    );
    return;
  }
  let report;
  try {
    report = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    add(
      errors,
      `${label} executionEvidence report is not valid JSON: ${evidence.report}.`,
    );
    return;
  }
  if (
    report.status !== "passed" &&
    report.status !== "passed-bounded-c2-native-slice"
  ) {
    add(
      errors,
      `${label} executionEvidence report is not an allowed passed result: ${evidence.report}.`,
    );
  }
  if (reportSourceDigest(report) !== evidence.sourceDigest) {
    add(
      errors,
      `${label} executionEvidence source digest does not match: ${evidence.report}.`,
    );
  }
  if (!Object.hasOwn(EXECUTION_CASES, evidence.case)) {
    add(
      errors,
      `${label} executionEvidence has an unknown bounded case: ${JSON.stringify(evidence.case)}.`,
    );
    return;
  }
  const definition = EXECUTION_CASES[evidence.case];
  if (!definition.scenarios.has(scenario.id)) {
    add(
      errors,
      `${label} executionEvidence case ${JSON.stringify(evidence.case)} is not valid for ${scenario.id}.`,
    );
    return;
  }
  if (!definition.proves(report)) {
    add(
      errors,
      `${label} executionEvidence report does not prove ${JSON.stringify(evidence.case)}.`,
    );
  }
}

function validateOracles(oracles, label, errors) {
  if (!isObject(oracles)) {
    add(errors, `${label} needs remote, local, and UI outcome oracles.`);
    return;
  }
  for (const surface of ["remote", "local", "ui"]) {
    const outcome = oracles[surface];
    if (
      !isObject(outcome) ||
      !OUTCOME_STATUSES.has(outcome.status) ||
      !nonEmptyString(outcome.expectation)
    ) {
      add(
        errors,
        `${label} needs a ${surface} outcome oracle with status and expectation.`,
      );
    }
    if (
      (outcome?.status === "not-applicable" ||
        outcome?.status === "not-defined") &&
      !nonEmptyString(outcome.reason)
    ) {
      add(
        errors,
        `${label} ${surface} ${outcome.status} outcome needs a reason.`,
      );
    }
  }
}

export function loadCatalogue(path = CATALOGUE_PATH) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function closingParen(source, openIndex) {
  let depth = 0;
  let quote = null;
  let escaped = false;
  for (let index = openIndex; index < source.length; index += 1) {
    const character = source[index];
    if (quote) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === quote) {
        quote = null;
      }
      continue;
    }
    if (character === '"' || character === "'" || character === "`") {
      quote = character;
    } else if (character === "(") {
      depth += 1;
    } else if (character === ")") {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

function declaresEachTest(source, symbol) {
  // Examine every `test.each` or `it.each` start independently. The table may
  // contain nested calls such as `.flatMap(...)`, so find its matching
  // delimiter rather than stopping at the first right parenthesis. The
  // negative lookbehind keeps `helper.it.each` and `object.test.each` from
  // being treated as Vitest globals.
  const starts = /(?<![\w$.])(?:test|it)\.each\(/g;
  for (const match of source.matchAll(starts)) {
    const tableOpen = match.index + match[0].length - 1;
    const tableClose = closingParen(source, tableOpen);
    if (tableClose < 0) continue;
    // A literal empty parameter table declares the callback in source, but
    // Vitest never invokes it. Do not let it count as executable evidence.
    if (source.slice(tableOpen + 1, tableClose).trim() === "[]") continue;
    let nameOpen = tableClose + 1;
    while (/\s/.test(source[nameOpen] ?? "")) nameOpen += 1;
    if (source[nameOpen] !== "(") continue;
    nameOpen += 1;
    while (/\s/.test(source[nameOpen] ?? "")) nameOpen += 1;
    const quote = source[nameOpen];
    if (!["'", '"', "`"].includes(quote)) continue;
    if (
      source.slice(nameOpen + 1, nameOpen + 1 + symbol.length) === symbol &&
      source[nameOpen + 1 + symbol.length] === quote
    ) {
      return true;
    }
  }
  return false;
}

function declaresTest(source, path, symbol) {
  const executableSource = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/[^\n]*/g, "");
  const escaped = escapeRegExp(symbol);
  if (path.endsWith(".rs")) {
    return new RegExp(
      `#\\[(?:tokio::)?test(?:\\([^\\]]*\\))?\\][\\s\\S]{0,400}?\\b(?:async\\s+)?fn\\s+${escaped}\\s*\\(`,
    ).test(executableSource);
  }
  const testFactory =
    "(?<![\\w$.])(?:test|it)(?:\\.(?!each\\b)[A-Za-z_][A-Za-z0-9_]*)*";
  const directDeclaration = new RegExp(
    `${testFactory}\\(\\s*["'\\x60]${escaped}["'\\x60]`,
  );
  // Vitest also supports a configured form such as
  // `it.skipIf(condition)("name", ...)`.  The test name remains a runtime
  // declaration, so accept that syntax without accepting a prose mention.
  const configuredDeclaration = new RegExp(
    `${testFactory}\\([^\\n]*\\)\\s*\\(\\s*["'\\x60]${escaped}["'\\x60]`,
  );
  // `it.each` and `test.each` often use a multiline table. The exact runtime
  // test name is still declared after the table, so recognize it without
  // accepting an arbitrary prose mention.
  return (
    directDeclaration.test(executableSource) ||
    configuredDeclaration.test(executableSource) ||
    declaresEachTest(executableSource, symbol)
  );
}

function sourceContains(root, evidence) {
  const path = resolve(root, evidence.path);
  if (!existsSync(path)) return `does not exist: ${evidence.path}`;
  const source = readFileSync(path, "utf8");
  if (!declaresTest(source, evidence.path, evidence.symbol)) {
    return `does not declare test ${JSON.stringify(evidence.symbol)}: ${evidence.path}`;
  }
  return null;
}

function sourceDeclaresEntrypoint(source, symbol) {
  const executableSource = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/[^\n]*/g, "");
  const escaped = escapeRegExp(symbol);
  return new RegExp(
    `(?:\\b(?:struct|enum|class|mod|fn|function|const)\\s+${escaped}\\b|\\b${escaped}!|::${escaped}\\b|["']${escaped}["'])`,
  ).test(executableSource);
}

function validateEntrypoint(root, label) {
  const reference = ENTRYPOINT_REFERENCES[label];
  if (!reference)
    return `has no declared production entrypoint reference: ${label}`;
  const [relativePath, symbol] = reference;
  const path = resolve(root, relativePath);
  if (!existsSync(path))
    return `entrypoint ${label} source does not exist: ${relativePath}`;
  if (!sourceDeclaresEntrypoint(readFileSync(path, "utf8"), symbol)) {
    return `entrypoint ${label} does not declare ${symbol} in ${relativePath}`;
  }
  return null;
}

export function validateCatalogue(catalogue, { root = process.cwd() } = {}) {
  const errors = [];
  if (!isObject(catalogue))
    return ["Mail safety catalogue must be a JSON object."];
  if (catalogue.schemaVersion !== 1)
    add(errors, "Mail safety catalogue schemaVersion must be 1.");
  if (typeof catalogue.title !== "string" || !catalogue.title.trim())
    add(errors, "Mail safety catalogue title must be a non-empty string.");
  if (!strings(catalogue.lanes) || catalogue.lanes.length === 0) {
    add(
      errors,
      "Mail safety catalogue lanes must be a non-empty string array.",
    );
  } else {
    for (const lane of catalogue.lanes)
      if (!LANES.has(lane)) add(errors, `Unknown catalogue lane: ${lane}`);
  }
  if (!Array.isArray(catalogue.scenarios) || catalogue.scenarios.length === 0) {
    add(errors, "Mail safety catalogue scenarios must be a non-empty array.");
    return errors;
  }

  const ids = new Set();
  const coveredAreas = new Set();
  for (const scenario of catalogue.scenarios) {
    if (!isObject(scenario)) {
      add(errors, "Every mail safety scenario must be an object.");
      continue;
    }
    const label = `Scenario ${scenario.id ?? "<missing id>"}`;
    if (
      typeof scenario.id !== "string" ||
      !/^MS-([A-L])-\d{3}$/.test(scenario.id)
    ) {
      add(errors, `${label} id must use MS-A-001 through MS-L-999.`);
    } else {
      const area = scenario.id.split("-")[1];
      coveredAreas.add(area);
      if (ids.has(scenario.id))
        add(errors, `Duplicate mail safety scenario id: ${scenario.id}`);
      ids.add(scenario.id);
    }
    if (typeof scenario.area !== "string" || !scenario.area.trim())
      add(errors, `${label} area must be a non-empty string.`);
    if (!PRIORITIES.has(scenario.priority))
      add(errors, `${label} priority must be P0, P1, or P2.`);
    if (typeof scenario.title !== "string" || !scenario.title.trim())
      add(errors, `${label} title must be a non-empty string.`);
    validateInput(scenario.input, label, errors, root);
    validateExecutionEvidence(
      scenario.executionEvidence,
      scenario,
      label,
      errors,
      root,
    );
    validateOracles(scenario.oracles, label, errors);
    if (
      !strings(scenario.entrypoints) ||
      scenario.entrypoints.length === 0 ||
      scenario.entrypoints.some((value) => !value.trim())
    ) {
      add(errors, `${label} needs at least one entrypoint.`);
    } else {
      for (const entrypoint of scenario.entrypoints) {
        const problem = validateEntrypoint(root, entrypoint);
        if (problem) add(errors, `${label} ${problem}.`);
      }
    }
    if (!strings(scenario.lanes) || scenario.lanes.length === 0) {
      add(errors, `${label} needs at least one lane.`);
    } else {
      for (const lane of scenario.lanes) {
        if (!LANES.has(lane) || !catalogue.lanes.includes(lane))
          add(errors, `${label} uses an undeclared lane: ${lane}.`);
      }
    }
    if (!STATUSES.has(scenario.status))
      add(
        errors,
        `${label} has unknown status ${JSON.stringify(scenario.status)}.`,
      );
    if (!Array.isArray(scenario.standards) || scenario.standards.length === 0) {
      add(errors, `${label} needs at least one standards reference.`);
    } else {
      for (const standard of scenario.standards) {
        if (
          !isObject(standard) ||
          !nonEmptyString(standard.id) ||
          !STANDARD_STATUSES.has(standard.status) ||
          !nonEmptyString(standard.requirement) ||
          !nonEmptyString(standard.profile) ||
          !nonEmptyString(standard.rationale)
        ) {
          add(
            errors,
            `${label} has an invalid standards requirement/profile reference.`,
          );
        }
      }
    }
    const evidence = scenario.evidence ?? [];
    if (scenario.status === "implemented") {
      if (!Array.isArray(evidence) || evidence.length === 0) {
        add(errors, `${label} is implemented but has no test evidence.`);
      } else {
        for (const item of evidence) {
          if (
            !isObject(item) ||
            typeof item.path !== "string" ||
            typeof item.symbol !== "string" ||
            !item.path ||
            !item.symbol
          ) {
            add(errors, `${label} has invalid evidence.`);
            continue;
          }
          const problem = sourceContains(root, item);
          if (problem) add(errors, `${label} evidence ${problem}`);
        }
        if (scenario.input?.kind === "source-test") {
          const sourceInput = scenario.input.id;
          const evidenceIds = evidence
            .filter(
              (item) =>
                isObject(item) &&
                typeof item.path === "string" &&
                typeof item.symbol === "string",
            )
            .map((item) => `${item.path}#${item.symbol}`);
          if (!evidenceIds.includes(sourceInput)) {
            add(
              errors,
              `${label} source-test input must name one of its implemented evidence tests.`,
            );
          }
        }
      }
    } else {
      if (Array.isArray(evidence) && evidence.length > 0)
        add(
          errors,
          `${label} is ${scenario.status} and must not be counted with evidence.`,
        );
      if (typeof scenario.gap !== "string" || !scenario.gap.trim())
        add(
          errors,
          `${label} is ${scenario.status} and must explain its limitation.`,
        );
    }
  }
  for (const area of AREAS)
    if (!coveredAreas.has(area))
      add(
        errors,
        `Mail safety catalogue has no scenario for programme area ${area}.`,
      );
  return errors;
}

function gitRevision(root) {
  try {
    return execFileSync("git", ["rev-parse", "HEAD"], {
      cwd: root,
      encoding: "utf8",
    }).trim();
  } catch {
    return "unavailable";
  }
}

export function reportCatalogue(
  catalogue,
  { root = process.cwd(), lane = "all" } = {},
) {
  const scenarios = catalogue.scenarios.filter(
    (scenario) => lane === "all" || scenario.lanes.includes(lane),
  );
  const counts = Object.fromEntries([...STATUSES].map((status) => [status, 0]));
  const traceability = { source: 0, fixture: 0, manual: 0, pending: 0 };
  const oracleStatus = { expected: 0, "not-applicable": 0, "not-defined": 0 };
  const p0Gaps = [];
  for (const scenario of scenarios) {
    counts[scenario.status] += 1;
    if (scenario.input?.kind === "source-test") traceability.source += 1;
    else if (scenario.input?.kind === "fixture") traceability.fixture += 1;
    else if (scenario.input?.kind === "manual-fixture")
      traceability.manual += 1;
    else if (scenario.input?.kind === "not-captured") traceability.pending += 1;
    for (const surface of ["remote", "local", "ui"]) {
      const status = scenario.oracles?.[surface]?.status;
      if (status in oracleStatus) oracleStatus[status] += 1;
    }
    if (scenario.priority === "P0" && scenario.status !== "implemented")
      p0Gaps.push(`${scenario.id}: ${scenario.title}`);
  }
  const lines = [
    "# Mail safety catalogue report",
    "",
    `- Revision: \`${gitRevision(root)}\``,
    `- Environment: Node ${process.version} on ${process.platform}/${process.arch}`,
    `- Requested lane: \`${lane}\``,
    `- Scenarios in report: ${scenarios.length}`,
    `- Implemented source evidence: ${counts.implemented}`,
    `- Gaps: ${counts.gap}`,
    `- Manual-required: ${counts["manual-required"]}`,
    `- Unsupported: ${counts.unsupported}`,
    `- Traceable inputs: ${traceability.source} source tests, ${traceability.fixture} checked-in fixtures, ${traceability.manual} manual fixtures, ${traceability.pending} not yet captured`,
    `- Outcome oracles: ${oracleStatus.expected} concrete expected test oracles, ${oracleStatus["not-applicable"]} not-applicable with reasons, ${oracleStatus["not-defined"]} still need refinement`,
    "",
    "This report validates catalogue declarations only. Outcome oracles are acceptance expectations, not proof that their named source test executed. It does not execute Rust, frontend, provider, native, or release tests; it must not be treated as a passing test result.",
  ];
  if (p0Gaps.length > 0) {
    lines.push(
      "",
      "## P0 work not counted as a pass",
      "",
      ...p0Gaps.map((item) => `- ${item}`),
    );
  }
  return `${lines.join("\n")}\n`;
}
