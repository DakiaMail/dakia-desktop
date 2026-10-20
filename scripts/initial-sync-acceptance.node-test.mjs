import assert from "node:assert/strict";
import test from "node:test";
import {
  acceptanceCases,
  parseArgs,
  parseMetricLines,
  reportFor,
  runAcceptanceCase,
} from "./initial-sync-acceptance.mjs";

test("acceptance cases state fixture conditions and measurement limits", () => {
  assert.deepEqual(
    acceptanceCases.map(({ id }) => id),
    [
      "initial-inbox-window",
      "snapshot-30000",
      "snapshot-100000",
      "fresh-catalogue-batching",
      "incomplete-snapshot-safety",
      "account-isolation-and-incremental-sync",
      "smtp-uncertain-delivery",
    ],
  );
  for (const entry of acceptanceCases) {
    assert.equal(Number.isInteger(entry.fixture.messages), true, entry.id);
    assert.equal(
      Number.isInteger(entry.fixture.injectedNetworkDelayMs),
      true,
      entry.id,
    );
    assert.ok(entry.proves.length > 0, entry.id);
    assert.ok(entry.doesNotProve.length > 0, entry.id);
  }
  assert.match(
    acceptanceCases
      .find(({ id }) => id === "snapshot-100000")
      .doesNotProve.join(" "),
    /peak memory/,
  );
});

test("argument parsing keeps case selection explicit", () => {
  assert.deepEqual(
    parseArgs(["--case", "snapshot-30000", "--output", "/tmp/result.json"]),
    {
      selected: ["snapshot-30000"],
      output: "/tmp/result.json",
      list: false,
    },
  );
  assert.throws(() => parseArgs(["--case"]), /requires/);
  assert.throws(() => parseArgs(["--unknown"]), /unknown argument/);
});

test("runner invokes one exact single-threaded production-path test", () => {
  const calls = [];
  const result = runAcceptanceCase(
    acceptanceCases[1],
    (command, args, options) => {
      calls.push({ command, args, options });
      return { status: 0, stdout: "test result: ok", stderr: "" };
    },
  );
  assert.equal(result.status, "passed");
  assert.equal(calls[0].command, "cargo");
  assert.deepEqual(calls[0].args, [
    "test",
    "--locked",
    "-p",
    "dakia-core",
    "mail::tests::scripted_thirty_thousand_message_snapshot_uses_sixty_bounded_pages",
    "--",
    "--exact",
    "--nocapture",
    "--test-threads=1",
  ]);
  assert.equal(calls[0].options.env.DAKIA_ACCEPTANCE_METRICS, "1");
});

test("runner parses only explicit machine-readable production metrics", () => {
  assert.deepEqual(
    parseMetricLines(
      'ordinary test output\nDAKIA_METRIC {"commands":2,"responseBytes":412}\n',
    ),
    [{ commands: 2, responseBytes: 412 }],
  );
  assert.throws(
    () => parseMetricLines("DAKIA_METRIC not-json"),
    /invalid DAKIA_METRIC/,
  );
});

test("report distinguishes failed checks from skipped or absent evidence", () => {
  const report = reportFor(
    [
      { id: "pass", status: "passed" },
      { id: "fail", status: "failed" },
    ],
    { platform: "fixture" },
  );
  assert.equal(report.allPassed, false);
  assert.deepEqual(
    report.results.map(({ status }) => status),
    ["passed", "failed"],
  );
});

test("runner classifies compilation blockers separately from acceptance failures", () => {
  const result = runAcceptanceCase(acceptanceCases[0], () => ({
    status: 101,
    stdout: "",
    stderr: "error[E0609]: no field\nerror: could not compile `dakia-core`",
  }));
  assert.equal(result.status, "failed");
  assert.equal(result.failureKind, "compilation");
  assert.deepEqual(result.metrics, []);
});
