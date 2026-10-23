import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  buildSample,
  buildSummary,
  validateOwner,
  loadIdentityRecord,
  validateSourceBinding,
  bundleInventory,
} from "./native-webkit-resource-collector.mjs";

const RUN_ID = "11111111-1111-4111-8111-111111111111";
const MAIN_PID = 4242;
const MAIN_START = "1000000";
const COLLECTOR = fileURLToPath(
  new URL("./native-webkit-resource-collector.mjs", import.meta.url),
);

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
}

function processInfo(overrides = {}) {
  return {
    pid: MAIN_PID,
    ppid: 1,
    uid: process.getuid(),
    comm: "dakia-desktop",
    name: "dakia-desktop",
    path: "/private/tmp/fake/Dakia.app/Contents/MacOS/dakia-desktop",
    startTimeMicros: MAIN_START,
    rssBytes: 500,
    virtualBytes: 2_000,
    threadCount: 4,
    openFileDescriptors: 5,
    ...overrides,
  };
}

function webKitProcess(overrides = {}) {
  return processInfo({
    pid: 4343,
    ppid: 1,
    comm: "com.apple.WebKit",
    name: "com.apple.WebKit.WebContent",
    path: "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent",
    startTimeMicros: "1000001",
    rssBytes: 200,
    virtualBytes: 1_000,
    threadCount: 2,
    openFileDescriptors: 3,
    responsiblePid: MAIN_PID,
    responsibilityVerified: true,
    identityStable: true,
    ...overrides,
  });
}

function responsibilityCensus(processes = [], overrides = {}) {
  return {
    availability: "lower_bound_only",
    reason: null,
    enumeratedPidCount: 50,
    responsibilityMatchedPidCount: processes.length,
    acceptedWebKitProcessCount: processes.length,
    processes,
    ...overrides,
  };
}

function unavailableCensus(reason = "private_api_unavailable", overrides = {}) {
  return responsibilityCensus([], {
    availability: "unavailable",
    reason,
    acceptedWebKitProcessCount: 0,
    ...overrides,
  });
}

function makeScenario({
  webKitProcesses = [],
  census = responsibilityCensus(webKitProcesses),
  main = {},
  limitOverrides = {},
  mutateSample,
} = {}) {
  const runRoot = mkdtempSync("/private/tmp/dakia-l013-resource-offline-test-");
  chmodSync(runRoot, 0o700);
  try {
    const markerPath = join(runRoot, ".dakia-l013-owned-run.json");
    writeJson(markerPath, {
      schemaVersion: 1,
      purpose: "dakia-native-l013-resource-v1",
      runId: RUN_ID,
      runRoot,
      ownerUid: process.getuid(),
    });
    const owner = validateOwner(runRoot);
    const applicationRoot = join(
      runRoot,
      "Dakia Fictional Provider Reader.app",
    );
    const executablePath = join(
      applicationRoot,
      "Contents/MacOS/dakia-desktop",
    );
    mkdirSync(join(applicationRoot, "Contents/MacOS"), { recursive: true });
    writeFileSync(executablePath, "fictional executable", { mode: 0o755 });
    const executableSha256 = sha256(executablePath);
    writeFileSync(
      join(applicationRoot, "Contents/Info.plist"),
      `<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-offline</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></plist>`,
      { mode: 0o600 },
    );
    const sourceRoot = join(runRoot, "source");
    mkdirSync(sourceRoot);
    writeFileSync(join(sourceRoot, "source.txt"), "fictional source");
    const sourceRow = {
      path: "source.txt",
      state: "present",
      sha256: sha256(join(sourceRoot, "source.txt")),
      bytes: 16,
    };
    const digest = createHash("sha256")
      .update("source.txt")
      .update("\0")
      .update(Buffer.from(sourceRow.sha256, "hex"))
      .update("\0")
      .digest("hex");
    const pin = (name, value) => {
      const path = join(runRoot, name);
      writeJson(path, value);
      return { path, sha256: sha256(path) };
    };
    const sourceManifest = pin("source-manifest.json", {
      algorithm: "sha256-path-and-content-manifest",
      digest,
      fileCount: 1,
      files: [sourceRow],
    });
    const entries = [
      {
        path: "Contents/MacOS/dakia-desktop",
        sha256: executableSha256,
        type: "file",
      },
    ];
    const inventoryDigest = createHash("sha256")
      .update(JSON.stringify(entries))
      .digest("hex");
    const approvedIdentity = pin("approved-native-identity.json", {
      schemaVersion: 1,
      contract: "dakia-native-provider-reader-approved-identity-v1",
      displayName: "Dakia Fictional Provider Reader",
      bundleIdentifier: "dev.dakia.fictional-provider-reader.run-offline",
      approvedRoot: runRoot,
      approvedApp: applicationRoot,
      inventory: { entries, digest: inventoryDigest },
    });
    const candidateTransaction = pin(".approved-native-identity-swap.json", {
      schemaVersion: 1,
      contract: "dakia-native-provider-reader-approved-identity-v1",
      transactionId: RUN_ID,
      stage: "candidate-installed",
      createdAt: new Date(0).toISOString(),
      approvedRoot: runRoot,
      approvedApp: applicationRoot,
      originalInventoryDigest: inventoryDigest,
      candidateInventoryDigest: bundleInventory(applicationRoot).digest,
      candidateDesktopSha256: executableSha256,
    });
    const webviewEvidence = pin("webview-evidence.json", {
      schemaVersion: 1,
      method: "native-accessibility-tree",
      pid: MAIN_PID,
      startTimeMicros: MAIN_START,
      executablePath,
      executableSha256,
      webviewUrl: "tauri://localhost/",
      observedAt: new Date().toISOString(),
    });
    const runtime = {
      schemaVersion: 1,
      purpose: "dakia-native-resource-runtime-v1",
      runId: RUN_ID,
      runRoot,
      ownerMarkerSha256: owner.markerSha256,
      source: {
        root: sourceRoot,
        digest,
        fileCount: 1,
        manifest: sourceManifest,
      },
      approvedIdentity,
      candidateTransaction,
      application: {
        path: applicationRoot,
        bundleIdentifier: "dev.dakia.fictional-provider-reader.run-offline",
        executable: executablePath,
        executableSha256,
      },
      process: { pid: MAIN_PID, startTimeMicros: MAIN_START },
      webviewEvidence,
    };
    const identityPath = join(runRoot, "runtime-manifest.json");
    writeJson(identityPath, runtime);
    const identitySha = sha256(identityPath);
    const identity = loadIdentityRecord(owner, identityPath, identitySha);

    const limitsValue = {
      schemaVersion: 1,
      purpose: "dakia-native-l013-limits-v1",
      runId: RUN_ID,
      maxMainRssBytes: 1_000,
      maxMainOpenFileDescriptors: 10,
      maxMappedWebKitAggregateRssBytes: 1_000,
      maxMappedWebKitAggregateOpenFileDescriptors: 10,
      maxMappedWebKitChildren: 5,
      regressionThresholdPercent: 20,
      ...limitOverrides,
    };
    const limitsPath = join(runRoot, "limits.json");
    writeJson(limitsPath, limitsValue);
    const limitsSha = sha256(limitsPath);

    const mainProcess = processInfo({ path: executablePath, ...main });
    const sample = buildSample({
      owner,
      identity,
      identitySha,
      limits: { path: limitsPath, sha256: limitsSha, value: limitsValue },
      phase: "offline-01",
      before: { main: mainProcess, responsibilityCensus: census },
      after: { main: mainProcess, responsibilityCensus: census },
    });
    if (mutateSample) mutateSample(sample);

    const evidenceRoot = join(runRoot, "evidence");
    mkdirSync(evidenceRoot, { mode: 0o700 });
    const samplePath = join(evidenceRoot, "sample.json");
    writeJson(samplePath, sample);
    const sampleListPath = join(runRoot, "sample-list.json");
    writeJson(sampleListPath, {
      schemaVersion: 1,
      purpose: "dakia-native-l013-sample-list-v1",
      runId: RUN_ID,
      identity: { path: identityPath, sha256: identitySha },
      limits: { path: limitsPath, sha256: limitsSha },
      phasePlan: [{ phase: "offline-01", exactSamples: 1 }],
      samples: [{ path: samplePath, sha256: sha256(samplePath) }],
      historicalBaseline: null,
    });
    const listRecord = {
      path: sampleListPath,
      sha256: sha256(sampleListPath),
      value: JSON.parse(readFileSync(sampleListPath, "utf8")),
    };
    return {
      runRoot,
      sample,
      listRecord,
      owner,
      runtime,
      identityPath,
      identitySha,
      summarize: () => buildSummary(owner, listRecord),
    };
  } catch (error) {
    rmSync(runRoot, { recursive: true, force: true });
    throw error;
  }
}

function usingScenario(options, assertion) {
  const scenario = makeScenario(options);
  try {
    assertion(scenario);
  } finally {
    rmSync(scenario.runRoot, { recursive: true, force: true });
  }
}

test("no responsibility-mapped WebKit processes remain null and unavailable in sample and summary", () => {
  usingScenario({}, ({ sample, summarize }) => {
    assert.deepEqual(sample.mappedWebKit.aggregate, {
      rssBytes: null,
      openFileDescriptors: null,
    });
    assert.equal(sample.mappedWebKit.metricAvailability, "unavailable");
    assert.deepEqual(
      sample.limits.checks
        .slice(2)
        .map(({ value, status }) => ({ value, status })),
      [
        { value: null, status: "unavailable" },
        { value: null, status: "unavailable" },
        { value: null, status: "unavailable" },
      ],
    );
    assert.equal(sample.limits.mainStatus, "passed");
    assert.equal(sample.limits.mappedWebKitStatus, "unavailable");

    const summary = summarize();
    assert.equal(summary.peakMetrics.mappedWebKitAggregateRssBytes, null);
    assert.equal(
      summary.peakMetrics.mappedWebKitAggregateOpenFileDescriptors,
      null,
    );
    assert.equal(summary.peakMetrics.mappedWebKitChildren, null);
    assert.equal(summary.absoluteLimits.mainStatus, "passed");
    assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unavailable");
    assert.equal(summary.absoluteLimits.status, "unavailable");
    assert.equal(summary.webKitScope.allWebKitMemoryClaimed, false);
  });
});

test("positive mappings below ceilings are lower bounds and remain unassessable", () => {
  usingScenario(
    { webKitProcesses: [webKitProcess()] },
    ({ sample, summarize }) => {
      assert.equal(sample.collection.allPidsEnumerated, true);
      assert.deepEqual(sample.mappedWebKit.aggregate, {
        rssBytes: 200,
        openFileDescriptors: 3,
      });
      assert.equal(sample.mappedWebKit.metricAvailability, "lower_bound_only");
      assert.deepEqual(
        sample.limits.checks
          .slice(2)
          .map(({ value, status, interpretation }) => ({
            value,
            status,
            interpretation,
          })),
        [
          {
            value: 200,
            status: "unassessable_below_ceiling",
            interpretation: "lower_bound_only",
          },
          {
            value: 3,
            status: "unassessable_below_ceiling",
            interpretation: "lower_bound_only",
          },
          {
            value: 1,
            status: "unassessable_below_ceiling",
            interpretation: "lower_bound_only",
          },
        ],
      );
      assert.equal(sample.limits.status, "unassessable");

      const summary = summarize();
      assert.equal(summary.peakMetrics.mappedWebKitAggregateRssBytes, 200);
      assert.equal(
        summary.peakMetrics.mappedWebKitAggregateOpenFileDescriptors,
        3,
      );
      assert.equal(summary.peakMetrics.mappedWebKitChildren, 1);
      assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unassessable");
      assert.equal(summary.absoluteLimits.status, "unassessable");
      assert.equal(summary.webKitScope.metricAvailability, "lower_bound_only");
    },
  );
});

test("a positive lower bound above a ceiling fails", () => {
  usingScenario(
    { webKitProcesses: [webKitProcess({ rssBytes: 1_001 })] },
    ({ sample, summarize }) => {
      assert.equal(sample.limits.checks[2].status, "failed");
      assert.equal(sample.limits.mappedWebKitStatus, "failed");
      assert.equal(sample.limits.status, "failed");
      const summary = summarize();
      assert.equal(summary.absoluteLimits.mappedWebKitStatus, "failed");
      assert.equal(summary.absoluteLimits.status, "failed");
    },
  );
});

test("a positive lower bound exactly at a ceiling remains unassessable", () => {
  usingScenario(
    { webKitProcesses: [webKitProcess({ rssBytes: 1_000 })] },
    ({ sample, summarize }) => {
      assert.equal(
        sample.limits.checks[2].status,
        "unassessable_below_ceiling",
      );
      assert.equal(sample.limits.mappedWebKitStatus, "unassessable");
      assert.equal(sample.limits.status, "unassessable");
      const summary = summarize();
      assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unassessable");
      assert.equal(summary.absoluteLimits.status, "unassessable");
    },
  );
});

test("main RSS and file descriptor ceilings remain complete checks", () => {
  usingScenario({ main: { rssBytes: 1_001 } }, ({ sample, summarize }) => {
    assert.equal(sample.limits.checks[0].status, "failed");
    assert.equal(sample.limits.checks[1].status, "passed");
    assert.equal(sample.limits.mainStatus, "failed");
    assert.equal(sample.limits.status, "failed");
    const summary = summarize();
    assert.equal(summary.absoluteLimits.mainStatus, "failed");
    assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unavailable");
    assert.equal(summary.absoluteLimits.status, "failed");
  });
});

test("summary rejects a forged numeric zero for absent WebKit mappings", () => {
  const scenario = makeScenario({
    mutateSample(sample) {
      sample.mappedWebKit.aggregate = { rssBytes: 0, openFileDescriptors: 0 };
    },
  });
  try {
    assert.throws(scenario.summarize, /WebKit aggregate is forged/);
  } finally {
    rmSync(scenario.runRoot, { recursive: true, force: true });
  }
});

test("summary rejects a forged completed all-PID census on unavailable evidence", () => {
  const scenario = makeScenario({
    census: unavailableCensus("private_api_unavailable"),
    mutateSample(sample) {
      sample.collection.allPidsEnumerated = true;
    },
  });
  try {
    assert.throws(scenario.summarize, /collection contract is invalid/);
  } finally {
    rmSync(scenario.runRoot, { recursive: true, force: true });
  }
});

for (const reason of [
  "private_api_unavailable",
  "private_api_call_failed",
  "main_responsibility_mismatch",
  "pid_enumeration_failed",
  "pid_enumeration_invalid",
  "pid_enumeration_truncated",
  "main_missing_from_pid_census",
  "responsibility_lookup_incomplete",
  "matched_process_unreadable_or_changed",
  "pid_census_unstable",
]) {
  test(`${reason} remains unavailable without a numeric WebKit zero`, () => {
    usingScenario(
      { census: unavailableCensus(reason) },
      ({ sample, summarize }) => {
        assert.equal(sample.collection.allPidsEnumerated, false);
        assert.equal(sample.mappedWebKit.census.reason, reason);
        assert.deepEqual(sample.mappedWebKit.processes, []);
        assert.deepEqual(sample.mappedWebKit.aggregate, {
          rssBytes: null,
          openFileDescriptors: null,
        });
        assert.equal(sample.mappedWebKit.metricAvailability, "unavailable");
        assert.equal(sample.limits.mappedWebKitStatus, "unavailable");
        const summary = summarize();
        assert.equal(summary.peakMetrics.mappedWebKitAggregateRssBytes, null);
        assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unavailable");
        assert.equal(summary.webKitScope.allWebKitMemoryClaimed, false);
      },
    );
  });
}

for (const [label, process] of [
  ["wrong responsible PID", webKitProcess({ responsiblePid: MAIN_PID + 1 })],
  [
    "unverified responsibility",
    webKitProcess({ responsibilityVerified: false }),
  ],
  ["unstable PID identity", webKitProcess({ identityStable: false })],
  [
    "process predating the main app",
    webKitProcess({ startTimeMicros: "999999" }),
  ],
  [
    "wrong WebKit path",
    webKitProcess({ path: "/private/tmp/com.apple.WebKit.WebContent" }),
  ],
  ["wrong WebKit name", webKitProcess({ name: "com.apple.WebKit.Networking" })],
]) {
  test(`candidate with ${label} is rejected instead of counted`, () => {
    assert.throws(
      () => makeScenario({ webKitProcesses: [process] }),
      /invalid WebKit process/,
    );
  });
}

test("sample persists no unrelated process identities or excluded PID list", () => {
  usingScenario({ webKitProcesses: [webKitProcess()] }, ({ sample }) => {
    assert.equal(sample.collection.argvEnumerated, false);
    assert.equal(sample.collection.environmentEnumerated, false);
    assert.equal(sample.collection.unrelatedProcessFilesRead, false);
    assert.equal(sample.collection.unrelatedProcessIdentitiesPersisted, false);
    assert.equal(
      sample.mappedWebKit.census.unrelatedProcessIdentitiesPersisted,
      false,
    );
    assert.equal(
      Object.hasOwn(sample.mappedWebKit, "observedDescendants"),
      false,
    );
    assert.equal(
      Object.hasOwn(sample.mappedWebKit, "excludedCandidates"),
      false,
    );
    assert.deepEqual(Object.keys(sample.mappedWebKit.census).sort(), [
      "acceptedWebKitProcessCount",
      "availability",
      "enumeratedPidCount",
      "reason",
      "responsibilityMatchedPidCount",
      "unrelatedProcessIdentitiesPersisted",
    ]);
  });
});

test("summary rejects added unrelated process evidence", () => {
  const scenario = makeScenario({
    mutateSample(sample) {
      sample.mappedWebKit.excludedCandidates = [
        { pid: 9999, reasons: ["unrelated"] },
      ];
    },
  });
  try {
    assert.throws(scenario.summarize, /persists unapproved process evidence/);
  } finally {
    rmSync(scenario.runRoot, { recursive: true, force: true });
  }
});

test("inconsistent census counts are rejected before a sample is built", () => {
  assert.throws(
    () =>
      makeScenario({
        webKitProcesses: [webKitProcess()],
        census: responsibilityCensus([webKitProcess()], {
          enumeratedPidCount: 0,
        }),
      }),
    /counts are inconsistent/,
  );
});

test("a successful all-PID census cannot claim that it enumerated zero PIDs", () => {
  assert.throws(
    () =>
      makeScenario({
        census: responsibilityCensus([], { enumeratedPidCount: 0 }),
      }),
    /Available responsibility census counts are invalid/,
  );
});

test("duplicate or unsorted mapped PIDs are rejected before persistence", () => {
  const first = webKitProcess({ pid: 5000 });
  const second = webKitProcess({ pid: 4000 });
  assert.throws(
    () =>
      makeScenario({
        webKitProcesses: [first, second],
        census: responsibilityCensus([first, second]),
      }),
    /not strictly sorted/,
  );
  assert.throws(
    () =>
      makeScenario({
        webKitProcesses: [first, first],
        census: responsibilityCensus([first, first]),
      }),
    /duplicate WebKit PIDs/,
  );
});

test("embedded native helper parses offline and uses only PID census before responsibility mapping", () => {
  const collectorSource = readFileSync(COLLECTOR, "utf8");
  const embedded = collectorSource.match(
    /const PYTHON_LIBPROC = String\.raw`\n([\s\S]*?)\n`;/,
  );
  assert.ok(embedded, "embedded Python collector was not found");
  const compiled = spawnSync(
    "/usr/bin/python3",
    [
      "-c",
      "import sys; compile(sys.stdin.read(), '<embedded-libproc>', 'exec')",
    ],
    {
      input: embedded[1],
      encoding: "utf8",
    },
  );
  assert.equal(compiled.status, 0, compiled.stderr);
  assert.match(embedded[1], /proc_listallpids/);
  assert.match(embedded[1], /responsibility_get_pid_responsible_for_pid/);
  assert.doesNotMatch(embedded[1], /proc_listchildpids/);
  assert.doesNotMatch(embedded[1], /subprocess|Popen|ps aux|sysctl/);
});

test("matched WebKit responsibility and identity are rechecked after resource measurement", () => {
  const collectorSource = readFileSync(COLLECTOR, "utf8");
  const embedded = collectorSource.match(
    /const PYTHON_LIBPROC = String\.raw`\n([\s\S]*?)\n`;/,
  )?.[1];
  assert.ok(embedded, "embedded Python collector was not found");
  const loop = embedded.match(
    /for pid in matches:([\s\S]*?)accepted\.append\(measured\)/,
  )?.[1];
  assert.ok(loop, "matched-process loop was not found");
  const measuredAt = loop.indexOf("measured = full(pid, first)");
  const responsibilityAt = loop.indexOf(
    "owner_after_measurement = int(responsible(pid))",
  );
  const finalIdentityAt = loop.indexOf("final_identity = identity(pid)");
  const comparisonAt = loop.indexOf("owner_after_measurement != root");
  assert.ok(measuredAt >= 0 && measuredAt < responsibilityAt);
  assert.ok(responsibilityAt < finalIdentityAt);
  assert.ok(finalIdentityAt < comparisonAt);
});

test("sample command pins and validates the main identity before the all-PID census", () => {
  const collectorSource = readFileSync(COLLECTOR, "utf8");
  const command = collectorSource.match(
    /function sampleCommand\(o\) \{([\s\S]*?)\n\}/,
  )?.[1];
  assert.ok(command, "sample command was not found");
  const pinnedAt = command.indexOf("loadIdentityRecord(");
  const mainReadAt = command.indexOf("readMainProcess(");
  const identityValidatedAt = command.indexOf("validateIdentity(");
  const censusAt = command.indexOf("readProcessCohort(");
  assert.ok(pinnedAt >= 0 && pinnedAt < mainReadAt);
  assert.ok(mainReadAt < identityValidatedAt);
  assert.ok(identityValidatedAt < censusAt);
  assert.doesNotMatch(command, /Number\(json\(o\.identity\)\.pid\)/);
});

test("historical baselines are preserved without establishing a new baseline or regression claim", () => {
  usingScenario({ webKitProcesses: [webKitProcess()] }, ({ summarize }) => {
    const summary = summarize();
    assert.equal(
      summary.regression.status,
      "not_evaluated_no_compatible_baseline",
    );
    assert.deepEqual(summary.regression.assessedMetrics, []);
    assert.match(summary.regression.claim, /no new or reset baseline/i);
  });
});

for (const [label, webKitProcesses, expectedStatus, expectedExit] of [
  ["unavailable", [], "unavailable", 3],
  ["unassessable", [webKitProcess()], "unassessable", 3],
  ["failed", [webKitProcess({ rssBytes: 1_001 })], "failed", 2],
]) {
  test(`${label} summaries return a distinct nonzero command exit`, () => {
    usingScenario({ webKitProcesses }, ({ runRoot, listRecord }) => {
      const output = join(runRoot, "evidence", `summary-${label}.json`);
      const result = spawnSync(
        process.execPath,
        [
          COLLECTOR,
          "summarize",
          "--run-root",
          runRoot,
          "--sample-list",
          listRecord.path,
          "--sample-list-sha",
          listRecord.sha256,
          "--output",
          output,
        ],
        { encoding: "utf8" },
      );
      assert.equal(result.status, expectedExit, result.stderr);
      assert.equal(
        JSON.parse(readFileSync(output, "utf8")).absoluteLimits.status,
        expectedStatus,
      );
    });
  });
}

function runtimePin(scenario, mutate) {
  const value = JSON.parse(readFileSync(scenario.identityPath, "utf8"));
  mutate(value);
  writeJson(scenario.identityPath, value);
  return sha256(scenario.identityPath);
}
function replacePinned(ref, mutate) {
  const value = JSON.parse(readFileSync(ref.path, "utf8"));
  mutate(value);
  writeJson(ref.path, value);
  ref.sha256 = sha256(ref.path);
}
function withLiveSource(scenario, assertion) {
  const previous = process.cwd();
  process.chdir(scenario.runtime.source.root);
  try {
    assertion({
      snapshot: () => scenario.runtime.source,
      now: Date.now(),
      live: true,
    });
  } finally {
    process.chdir(previous);
  }
}

test("runtime loader accepts a declared current cohort instead of a historical file count or digest", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      const identity = loadIdentityRecord(
        s.owner,
        s.identityPath,
        s.identitySha,
        options,
      );
      assert.equal(identity.provenance.source.fileCount, 1);
      assert.equal(identity.provenance.source.digest, s.runtime.source.digest);
      assert.equal(identity.provenance.webviewUrl, "tauri://localhost/");
      assert.equal(identity.executableSha256, sha256(identity.executablePath));
    }),
  );
});

test("governed-source enumeration detects omitted or newly added files even when listed bytes agree", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      assert.throws(
        () =>
          loadIdentityRecord(s.owner, s.identityPath, s.identitySha, {
            ...options,
            snapshot: () => ({ digest: "b".repeat(64), fileCount: 2 }),
          }),
        /no longer the current governed checkout/,
      );
    }),
  );
});

test("changed source bytes are rejected before process resources are sampled", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      writeFileSync(
        join(s.runtime.source.root, "source.txt"),
        "changed source",
      );
      assert.throws(
        () =>
          loadIdentityRecord(s.owner, s.identityPath, s.identitySha, options),
        /Source file changed/,
      );
    }),
  );
});

for (const [label, mutate, pattern] of [
  [
    "runtime owner",
    (r) => {
      r.ownerMarkerSha256 = "f".repeat(64);
    },
    /owned run/,
  ],
  [
    "old process from before candidate transaction",
    (r) =>
      replacePinned(r.candidateTransaction, (v) => {
        v.createdAt = new Date(2000).toISOString();
      }),
    /predates the candidate transaction/,
  ],
  [
    "runtime PID",
    (r) => {
      r.process.pid = 0;
    },
    /process identity/,
  ],
  [
    "runtime start identity",
    (r) => {
      r.process.startTimeMicros = "-1";
    },
    /process identity/,
  ],
  [
    "application path",
    (r) => {
      r.application.path = "/Applications/Dakia.app";
    },
    /executable binding/,
  ],
  [
    "bundle identifier",
    (r) => {
      r.application.bundleIdentifier = "dev.unrelated.app";
    },
    /executable binding/,
  ],
  [
    "source count",
    (r) => {
      r.source.fileCount++;
    },
    /Source manifest binding/,
  ],
  [
    "source digest",
    (r) => {
      r.source.digest = "f".repeat(64);
    },
    /Source manifest binding/,
  ],
  [
    "original inventory",
    (r) =>
      replacePinned(r.approvedIdentity, (v) => {
        v.inventory.digest = "f".repeat(64);
      }),
    /inventory digest/,
  ],
  [
    "transaction stage",
    (r) =>
      replacePinned(r.candidateTransaction, (v) => {
        v.stage = "original-retained";
      }),
    /Candidate transaction/,
  ],
  [
    "candidate executable hash",
    (r) =>
      replacePinned(r.candidateTransaction, (v) => {
        v.candidateDesktopSha256 = "f".repeat(64);
      }),
    /executable binding/,
  ],
  [
    "webview URL",
    (r) =>
      replacePinned(r.webviewEvidence, (v) => {
        v.webviewUrl = "https://example.invalid/";
      }),
    /webview evidence identity/,
  ],
  [
    "webview process",
    (r) =>
      replacePinned(r.webviewEvidence, (v) => {
        v.pid++;
      }),
    /webview evidence identity/,
  ],
  [
    "webview start identity",
    (r) =>
      replacePinned(r.webviewEvidence, (v) => {
        v.startTimeMicros = "999";
      }),
    /webview evidence identity/,
  ],
  [
    "webview executable hash",
    (r) =>
      replacePinned(r.webviewEvidence, (v) => {
        v.executableSha256 = "f".repeat(64);
      }),
    /webview evidence identity/,
  ],
  [
    "webview observer",
    (r) =>
      replacePinned(r.webviewEvidence, (v) => {
        v.method = "display-name-only";
      }),
    /webview evidence identity/,
  ],
]) {
  test(`runtime loader rejects changed ${label}`, () => {
    usingScenario({}, (s) => {
      const digest = runtimePin(s, mutate);
      assert.throws(
        () => loadIdentityRecord(s.owner, s.identityPath, digest),
        pattern,
      );
    });
  });
}

for (const [label, observedAt] of [
  ["stale", new Date(Date.now() - 180_000).toISOString()],
  ["future", new Date(Date.now() + 180_000).toISOString()],
]) {
  test(`${label} webview evidence cannot support a current sample`, () => {
    usingScenario({}, (s) =>
      withLiveSource(s, (options) => {
        const digest = runtimePin(s, (r) =>
          replacePinned(r.webviewEvidence, (v) => {
            v.observedAt = observedAt;
          }),
        );
        assert.throws(
          () => loadIdentityRecord(s.owner, s.identityPath, digest, options),
          /fresh within 120 seconds/,
        );
      }),
    );
  });
}

test("changed executable bytes are rejected even if the process path is unchanged", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      writeFileSync(s.runtime.application.executable, "wrong executable");
      assert.throws(
        () =>
          loadIdentityRecord(s.owner, s.identityPath, s.identitySha, options),
        /executable bytes changed/,
      );
    }),
  );
});

test("refreshing pinned webview observations preserves one runtime identity across phases", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      const path = join(s.runRoot, "webview-refreshed.json");
      const web = JSON.parse(
        readFileSync(s.runtime.webviewEvidence.path, "utf8"),
      );
      web.observedAt = new Date().toISOString();
      writeJson(path, web);
      const identity = loadIdentityRecord(
        s.owner,
        s.identityPath,
        s.identitySha,
        { ...options, webviewEvidence: { path, sha256: sha256(path) } },
      );
      assert.equal(identity.provenance.runtimeManifest.sha256, s.identitySha);
      assert.equal(identity.provenance.webviewEvidence.path, path);
    }),
  );
});

test("a retained candidate-journal copy supports summary after active transaction removal", () => {
  usingScenario({}, (s) => {
    const path = join(s.runRoot, "retained-candidate-transaction.json");
    writeFileSync(path, readFileSync(s.runtime.candidateTransaction.path), {
      mode: 0o600,
    });
    const digest = runtimePin(s, (r) => {
      r.candidateTransaction = {
        path,
        sha256: sha256(path),
        activePath: r.candidateTransaction.path,
      };
    });
    rmSync(s.runtime.candidateTransaction.path);
    const identity = loadIdentityRecord(s.owner, s.identityPath, digest);
    assert.equal(identity.provenance.candidateTransaction.path, path);
    withLiveSource(s, (options) =>
      assert.throws(
        () => loadIdentityRecord(s.owner, s.identityPath, digest, options),
        /ENOENT/,
      ),
    );
  });
});

test("summary rejects a sample whose source provenance was replaced", () => {
  usingScenario(
    {
      mutateSample(sample) {
        sample.provenance.source.digest = "f".repeat(64);
      },
    },
    (s) => {
      assert.throws(s.summarize, /runtime provenance changed/);
    },
  );
});

test("runtime record and pinned evidence reject permissive modes and symlinks", () => {
  usingScenario({}, (s) => {
    chmodSync(s.runtime.webviewEvidence.path, 0o644);
    assert.throws(
      () => loadIdentityRecord(s.owner, s.identityPath, s.identitySha),
      /mode 0600/,
    );
  });
});

for (const operation of ["add", "change", "remove"]) {
  test(`live bundle inventory detects ${operation} of a resource with an unchanged executable`, () => {
    usingScenario({}, (s) =>
      withLiveSource(s, (options) => {
        const path = join(
          s.runtime.application.path,
          "Contents",
          "resource.txt",
        );
        if (operation !== "add") writeFileSync(path, "original");
        const digest = runtimePin(s, (r) =>
          replacePinned(r.candidateTransaction, (v) => {
            v.candidateInventoryDigest = bundleInventory(
              s.runtime.application.path,
            ).digest;
          }),
        );
        if (operation === "remove") rmSync(path);
        else writeFileSync(path, "changed");
        assert.throws(
          () => loadIdentityRecord(s.owner, s.identityPath, digest, options),
          /bundle inventory changed/,
        );
      }),
    );
  });
}

test("sequential WebKit process handoff cannot create a false aggregate ceiling failure", () => {
  usingScenario(
    {
      webKitProcesses: [
        webKitProcess({ pid: 4343, rssBytes: 800, openFileDescriptors: 8 }),
        webKitProcess({ pid: 4344, rssBytes: 800, openFileDescriptors: 8 }),
      ],
    },
    ({ sample, summarize }) => {
      // The first process could release everything before the second acquires it.
      // Their sum 1600/16 is not a lower bound for any concurrent cohort.
      assert.equal(sample.mappedWebKit.processes.length, 2);
      assert.deepEqual(sample.mappedWebKit.aggregate, {
        rssBytes: 800,
        openFileDescriptors: 8,
      });
      assert.equal(sample.limits.checks[4].value, 1);
      assert.equal(sample.limits.mappedWebKitStatus, "unassessable");
      const summary = summarize();
      assert.equal(summary.peakMetrics.mappedWebKitAggregateRssBytes, 800);
      assert.equal(
        summary.peakMetrics.mappedWebKitAggregateOpenFileDescriptors,
        8,
      );
      assert.equal(summary.peakMetrics.mappedWebKitChildren, 1);
      assert.equal(summary.absoluteLimits.mappedWebKitStatus, "unassessable");
    },
  );
});
for (const ageMillis of [-1000, 121000]) {
  test(`summary rejects webview chronology offset ${ageMillis} ms relative to capturedAt`, () => {
    usingScenario(
      {
        mutateSample(s) {
          s.capturedAt = new Date(
            Date.parse(s.provenance.webviewObservedAt) + ageMillis,
          ).toISOString();
        },
      },
      ({ summarize }) => {
        assert.throws(summarize, /webview evidence is stale or newer/);
      },
    );
  });
}

test("candidate inventory contract agrees with the production transaction implementation", async () => {
  const source = readFileSync(
    new URL("./native-provider-reader-acceptance.sh", import.meta.url),
    "utf8",
  );
  const producer = source.match(
    /async function inventory\(root\) \{[\s\S]*?\n\}\n\n(?=async function readBundleMetadata)/,
  )?.[0];
  assert.ok(
    producer,
    "production transaction inventory implementation must be found",
  );
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const productionInventory = new AsyncFunction(
    "uid",
    "lstat",
    "readdir",
    "readlink",
    "sha256",
    "relative",
    "join",
    "createHash",
    "root",
    `${producer}; return inventory(root);`,
  );
  const root = mkdtempSync("/private/tmp/dakia-resource-inventory-contract-");
  try {
    mkdirSync(join(root, "nested-é"), { mode: 0o750 });
    writeFileSync(
      join(root, "nested-é", "asset.bin"),
      Buffer.from([0, 1, 255]),
      { mode: 0o640 },
    );
    symlinkSync("nested-é/asset.bin", join(root, "asset-link"));
    const fs = await import("node:fs/promises");
    const produced = await productionInventory(
      process.getuid(),
      fs.lstat,
      fs.readdir,
      fs.readlink,
      sha256,
      relative,
      join,
      createHash,
      root,
    );
    assert.deepEqual(bundleInventory(root), produced);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("freshness is rechecked after bounded bundle validation near the 120-second limit", () => {
  usingScenario({}, (s) =>
    withLiveSource(s, (options) => {
      const observedAt = Date.parse(
        JSON.parse(readFileSync(s.runtime.webviewEvidence.path, "utf8"))
          .observedAt,
      );
      let clockCalls = 0;
      assert.throws(
        () =>
          loadIdentityRecord(s.owner, s.identityPath, s.identitySha, {
            ...options,
            now: () => observedAt + (++clockCalls === 1 ? 119000 : 121000),
          }),
        /fresh within 120 seconds/,
      );
      assert.equal(clockCalls, 2);
    }),
  );
});
