#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { arch, platform, release } from "node:os";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));

export const acceptanceCases = Object.freeze([
  {
    id: "initial-inbox-window",
    test: "mail::tests::initial_inbox_publishes_a_batched_uid_associated_window_before_inventory",
    fixture: {
      messages: 5,
      uidShape: "sparse and out of response order",
      transport: "in-process duplex",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: false,
    },
    proves: [
      "initial Inbox uses EXAMINE and one recent sequence window",
      "headers are associated and persisted by UID rather than response order",
      "initial publication does not issue UID inventory or preview commands",
    ],
    doesNotProve: [
      "authentication-to-first-commit under injected latency",
      "React visibility or Tauri event delivery",
      "historical work continues after the initial method returns",
    ],
  },
  {
    id: "snapshot-30000",
    test: "mail::tests::scripted_thirty_thousand_message_snapshot_uses_sixty_bounded_pages",
    fixture: {
      messages: 30_000,
      uidShape: "dense",
      transport: "in-process duplex",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: true,
    },
    proves: [
      "complete UID membership is fetched in 60 bounded 500-message pages",
      "the production sync path avoids UID SEARCH ALL and an unbounded 1:* FETCH",
    ],
    doesNotProve: [
      "first-message latency",
      "header download cost for a fresh 30,000-message catalogue",
      "peak parser or process memory",
    ],
  },
  {
    id: "snapshot-100000",
    test: "mail::tests::scripted_one_hundred_thousand_message_snapshot_uses_bounded_pages",
    fixture: {
      messages: 100_000,
      uidShape: "dense",
      transport: "in-process duplex",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: true,
    },
    proves: [
      "complete UID membership is fetched in bounded 500-message pages",
      "the production sync path avoids UID SEARCH ALL and an unbounded 1:* FETCH",
    ],
    doesNotProve: [
      "bounded peak memory",
      "fresh-catalogue header publication",
      "behavior with sparse UIDs or provider latency",
    ],
  },
  {
    id: "fresh-catalogue-batching",
    test: "mail::tests::scripted_fresh_catalogue_batches_headers_and_snippets_by_uid_set",
    fixture: {
      messages: 3,
      uidShape: "dense",
      transport: "in-process duplex",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: false,
    },
    proves: [
      "fresh metadata uses one UID-set FETCH instead of one FETCH per message",
      "snippet previews use one UID-set FETCH for the fixture batch",
      "responses are persisted by UID through the production sync path",
    ],
    doesNotProve: [
      "Inbox publication before historical inventory",
      "visibility before the sync future finishes",
      "preview scheduling independence",
    ],
  },
  {
    id: "incomplete-snapshot-safety",
    test: "mail::tests::scripted_incomplete_snapshot_preserves_preexisting_mailbox_rows",
    fixture: {
      messages: 2,
      uidShape: "dense",
      transport: "in-process duplex",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: true,
    },
    proves: [
      "an incomplete membership snapshot does not delete committed rows",
    ],
    doesNotProve: [
      "reconciliation is fenced to the captured upper UID boundary",
      "a newer local mutation survives finalization",
    ],
  },
  {
    id: "account-isolation-and-incremental-sync",
    test: "mail::tests::scripted_mail_service_inbox_sync_persists_incremental_uid_and_isolates_accounts",
    fixture: {
      messages: 2,
      uidShape: "dense",
      transport: "loopback TCP",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: false,
    },
    proves: [
      "the production MailService persists an incremental UID",
      "the same mailbox UID remains isolated across accounts",
    ],
    doesNotProve: [
      "concurrent account sync",
      "account removal while a late worker publishes",
    ],
  },
  {
    id: "smtp-uncertain-delivery",
    test: "mail::tests::scripted_smtp_starttls_eof_after_data_reports_uncertain_delivery",
    fixture: {
      messages: 1,
      uidShape: "not applicable",
      transport: "loopback TCP with test TLS",
      injectedNetworkDelayMs: 0,
      preexistingCatalog: false,
    },
    proves: [
      "an EOF after DATA is reported as uncertain rather than safe to retry",
    ],
    doesNotProve: [
      "SMTP progresses while historical IMAP is stalled",
      "submission state survives restart",
      "SMTP acceptance and Sent-copy failure are persisted separately",
    ],
  },
]);

export function parseArgs(argv) {
  const selected = [];
  let output;
  let list = false;
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--case") {
      const id = argv[++index];
      if (!id) throw new Error("--case requires an acceptance case id");
      selected.push(id);
    } else if (value === "--output") {
      output = argv[++index];
      if (!output) throw new Error("--output requires a file path");
    } else if (value === "--list") {
      list = true;
    } else {
      throw new Error(`unknown argument: ${value}`);
    }
  }
  return { selected, output, list };
}

export function parseMetricLines(output) {
  return output
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.startsWith("DAKIA_METRIC "))
    .map((line) => {
      try {
        return JSON.parse(line.slice("DAKIA_METRIC ".length));
      } catch (error) {
        throw new Error(`invalid DAKIA_METRIC line: ${error.message}`);
      }
    });
}

function commandOutput(command, args) {
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return result.status === 0 ? result.stdout.trim() : null;
}

export function runAcceptanceCase(entry, spawn = spawnSync) {
  const args = [
    "test",
    "--locked",
    "-p",
    "dakia-core",
    entry.test,
    "--",
    "--exact",
    "--nocapture",
    "--test-threads=1",
  ];
  const started = performance.now();
  const result = spawn("cargo", args, {
    cwd: root,
    encoding: "utf8",
    env: { ...process.env, DAKIA_ACCEPTANCE_METRICS: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const durationMs = Math.round((performance.now() - started) * 100) / 100;
  const combinedOutput = `${result.stdout}\n${result.stderr}`;
  return {
    ...entry,
    command: ["cargo", ...args],
    status: result.status === 0 ? "passed" : "failed",
    failureKind:
      result.status !== 0 &&
      /could not compile|error\[E\d+\]/.test(combinedOutput)
        ? "compilation"
        : null,
    exitCode: result.status,
    durationMs,
    metrics: parseMetricLines(combinedOutput),
    stdout: result.stdout,
    stderr: result.stderr,
  };
}

export function reportFor(results, environment) {
  return {
    schemaVersion: 1,
    measuredAt: new Date().toISOString(),
    environment,
    results,
    allPassed: results.every((result) => result.status === "passed"),
  };
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.list) {
    process.stdout.write(`${acceptanceCases.map(({ id }) => id).join("\n")}\n`);
    return;
  }
  const unknown = options.selected.filter(
    (id) => !acceptanceCases.some((entry) => entry.id === id),
  );
  if (unknown.length)
    throw new Error(`unknown acceptance case: ${unknown.join(", ")}`);
  const cases = options.selected.length
    ? acceptanceCases.filter((entry) => options.selected.includes(entry.id))
    : acceptanceCases;
  const environment = {
    platform: platform(),
    release: release(),
    arch: arch(),
    rustc: commandOutput("rustc", ["--version"]),
    cargo: commandOutput("cargo", ["--version"]),
    gitCommit: commandOutput("git", ["rev-parse", "HEAD"]),
  };
  const results = [];
  for (let index = 0; index < cases.length; index += 1) {
    const entry = cases[index];
    const result = runAcceptanceCase(entry);
    process.stderr.write(
      `${entry.id}: ${result.status} (${result.durationMs} ms)\n`,
    );
    results.push(result);
    if (result.failureKind === "compilation") {
      for (const blocked of cases.slice(index + 1)) {
        results.push({
          ...blocked,
          status: "blocked",
          failureKind: "prior-compilation-failure",
          exitCode: null,
          durationMs: null,
          metrics: [],
          stdout: "",
          stderr: `blocked after ${entry.id} could not compile`,
        });
      }
      break;
    }
  }
  const report = reportFor(results, environment);
  const serialized = `${JSON.stringify(report, null, 2)}\n`;
  if (options.output) writeFileSync(options.output, serialized);
  else process.stdout.write(serialized);
  if (!report.allPassed) process.exitCode = 1;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1])
  main();
