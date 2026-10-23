import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  loadCatalogue,
  reportCatalogue,
  validateCatalogue,
  reportSourceDigest,
  isOwnedRegularReport,
} from "./mail-safety-catalogue.mjs";

const root =
  process.env.DAKIA_CATALOGUE_TEST_ROOT ??
  new URL("..", import.meta.url).pathname;

test("checked-in mail safety catalogue has traceable source evidence and every programme area", () => {
  const errors = validateCatalogue(
    loadCatalogue(join(root, "testdata/mail-safety-catalogue.json")),
    { root },
  );
  assert.deepEqual(errors, []);
});

test("manual execution evidence binds report bytes, source identity, and named outcomes", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const nativeReport =
    "docs/mail-safety-results/native-mime-attachment-terminal-restart-c2-2026-10-22.json";
  const nativeSha =
    "c44db66723c4bd48ef024c95983020ec49f80a19a1f5fe30f1e002a4bcf2585f";
  const digest =
    "c2e8ba357d822b2ef086e497916e77f6555a422605e7a4b99acfb1e81e379888";
  const withEvidence = (executionEvidence) => ({
    ...catalogue,
    scenarios: catalogue.scenarios.map((scenario) =>
      scenario.id === "MS-G-004"
        ? { ...scenario, executionEvidence }
        : scenario,
    ),
  });
  const valid = {
    case: "native-attachment-quarantine",
    report: nativeReport,
    reportSha256: nativeSha,
    sourceDigest: digest,
  };
  assert.deepEqual(validateCatalogue(withEvidence(valid), { root }), []);
  assert.match(
    validateCatalogue(
      withEvidence({
        ...valid,
        report:
          "docs/mail-safety-results/c2-deep-independent-servers-and-fuzz-2026-10-22.json",
        reportSha256:
          "9af1245ba25a7b91f0d82c3502a564a2313f2a0e4dde4d8ee129ffebb2576246",
      }),
      { root },
    ).join("\n"),
    /does not prove "native-attachment-quarantine"/,
  );
  assert.match(
    validateCatalogue(
      withEvidence({ ...valid, reportSha256: "0".repeat(64) }),
      { root },
    ).join("\n"),
    /report SHA-256 does not match/,
  );
  assert.match(
    validateCatalogue(
      withEvidence({ ...valid, sourceDigest: "0".repeat(64) }),
      { root },
    ).join("\n"),
    /source digest does not match/,
  );
});

test("execution evidence rejects prototype cases, case swapping, and contradictory after identities", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const native = {
    case: "native-attachment-quarantine",
    report:
      "docs/mail-safety-results/native-mime-attachment-terminal-restart-c2-2026-10-22.json",
    reportSha256:
      "c44db66723c4bd48ef024c95983020ec49f80a19a1f5fe30f1e002a4bcf2585f",
    sourceDigest:
      "c2e8ba357d822b2ef086e497916e77f6555a422605e7a4b99acfb1e81e379888",
  };
  const withG004 = (executionEvidence) => ({
    ...catalogue,
    scenarios: catalogue.scenarios.map((scenario) =>
      scenario.id === "MS-G-004"
        ? { ...scenario, executionEvidence }
        : scenario,
    ),
  });
  assert.match(
    validateCatalogue(withG004({ ...native, case: "constructor" }), {
      root,
    }).join("\n"),
    /unknown bounded case/,
  );
  assert.match(
    validateCatalogue(
      withG004({
        ...native,
        case: "dovecot-roundtrip",
        report:
          "docs/mail-safety-results/c2-deep-independent-servers-and-fuzz-2026-10-22.json",
        reportSha256:
          "9af1245ba25a7b91f0d82c3502a564a2313f2a0e4dde4d8ee129ffebb2576246",
      }),
      { root },
    ).join("\n"),
    /is not valid for MS-G-004/,
  );
  assert.equal(
    reportSourceDigest({
      sourceTree: { digest: native.sourceDigest },
      sourceTreeAfter: { digest: "0".repeat(64) },
    }),
    null,
  );
  assert.equal(isOwnedRegularReport(root, native.report), true);
});

test("implemented Rust evidence accepts configured tokio tests but rejects a missing symbol", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-mail-safety-catalogue-"));
  try {
    writeFileSync(
      join(directory, "test.rs"),
      '#[tokio::test(flavor = \\"multi_thread\\")] async fn present() {}\n',
    );
    const catalogue = {
      schemaVersion: 1,
      title: "fixture",
      lanes: ["pull-request"],
      scenarios: "ABCDEFGHIJKL".split("").map((area) => ({
        id: `MS-${area}-001`,
        area,
        priority: "P1",
        title: area,
        standards: [{ id: "fixture", status: "partial" }],
        lanes: ["pull-request"],
        entrypoints: ["fixture"],
        status: "implemented",
        evidence: [
          { path: "test.rs", symbol: area === "A" ? "absent" : "present" },
        ],
      })),
    };
    assert.match(
      validateCatalogue(catalogue, { root: directory }).join("\n"),
      /does not declare test "absent"/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("catalogue entrypoints must name a declared production boundary", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const scenario = catalogue.scenarios.find(({ id }) => id === "MS-B-012");
  assert.ok(scenario);
  const errors = validateCatalogue(
    {
      ...catalogue,
      scenarios: catalogue.scenarios.map((item) =>
        item.id === scenario.id
          ? { ...item, entrypoints: ["MailService::full_snapshot"] }
          : item,
      ),
    },
    { root },
  );
  assert.match(
    errors.join("\n"),
    /has no declared production entrypoint reference: MailService::full_snapshot/,
  );
});

test("gaps cannot carry evidence and reports retain them outside passing source evidence", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const report = reportCatalogue(catalogue, { root, lane: "pull-request" });
  assert.match(report, /Implemented source evidence:/);
  const gap = catalogue.scenarios.find((scenario) => scenario.status === "gap");
  assert.ok(gap);
  const p0Report = reportCatalogue(
    { ...catalogue, scenarios: [{ ...gap, priority: "P0" }] },
    { root, lane: "pull-request" },
  );
  assert.match(p0Report, /P0 work not counted as a pass/);
  assert.match(
    validateCatalogue({
      ...catalogue,
      scenarios: [{ ...gap, evidence: [{ path: "test", symbol: "test" }] }],
      lanes: catalogue.lanes,
    }).join("\n"),
    /must not be counted with evidence/,
  );
});

test("report rejects an undeclared lane instead of reporting an empty pass", () => {
  const result = spawnSync(
    process.execPath,
    ["scripts/report-mail-safety-catalogue.mjs", "--lane", "invented"],
    { cwd: root, encoding: "utf8" },
  );
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Unknown mail safety lane: invented/);
});

test("a comment that repeats a test name cannot satisfy an evidence reference", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-mail-safety-comment-"));
  try {
    writeFileSync(
      join(directory, "test.rs"),
      "// #[test] fn protected_test() {}\n",
    );
    const scenario = {
      id: "MS-A-001",
      area: "connection",
      priority: "P1",
      title: "fixture",
      standards: [{ id: "fixture", status: "partial" }],
      lanes: ["pull-request"],
      entrypoints: ["fixture"],
      status: "implemented",
      evidence: [{ path: "test.rs", symbol: "protected_test" }],
    };
    const catalogue = {
      schemaVersion: 1,
      title: "fixture",
      lanes: ["pull-request"],
      scenarios: "ABCDEFGHIJKL".split("").map((area) =>
        area === "A"
          ? scenario
          : {
              ...scenario,
              id: `MS-${area}-001`,
              area,
              status: "gap",
              evidence: undefined,
              gap: "fixture",
            },
      ),
    };
    assert.match(
      validateCatalogue(catalogue, { root: directory }).join("\n"),
      /does not declare test "protected_test"/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("implemented source inputs must identify one of the declared evidence tests", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const errors = validateCatalogue(
    {
      ...catalogue,
      scenarios: catalogue.scenarios.map((scenario) =>
        scenario.id === "MS-C-003"
          ? {
              ...scenario,
              input: {
                ...scenario.input,
                id: "apps/desktop/src/apiSendMessage.test.ts#missing",
              },
            }
          : scenario,
      ),
    },
    { root },
  ).join("\n");
  assert.match(
    errors,
    /source-test input must name one of its implemented evidence tests/,
  );
});

test("multiline Vitest parameter tables require the exact runtime test name", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-mail-safety-each-test-"));
  try {
    writeFileSync(
      join(directory, "test.ts"),
      'it.each([\n  ["delivery_uncertain"],\n])(\n  "keeps retry blocked after %s",\n  () => {},\n);\n',
    );
    const scenario = {
      id: "MS-A-001",
      area: "connection",
      priority: "P1",
      title: "fixture",
      standards: [
        {
          id: "fixture",
          status: "partial",
          requirement: "fixture",
          profile: "fixture",
          rationale: "fixture",
        },
      ],
      lanes: ["pull-request"],
      entrypoints: ["fixture"],
      status: "implemented",
      evidence: [{ path: "test.ts", symbol: "keeps retry blocked after %s" }],
      input: {
        kind: "source-test",
        id: "test.ts#keeps retry blocked after %s",
      },
      oracles: {
        remote: {
          status: "not-applicable",
          expectation: "fixture",
          reason: "fixture",
        },
        local: { status: "expected", expectation: "fixture" },
        ui: {
          status: "not-applicable",
          expectation: "fixture",
          reason: "fixture",
        },
      },
    };
    const catalogue = {
      schemaVersion: 1,
      title: "fixture",
      lanes: ["pull-request"],
      scenarios: "ABCDEFGHIJKL".split("").map((area) =>
        area === "A"
          ? scenario
          : {
              ...scenario,
              id: `MS-${area}-001`,
              area,
              status: "gap",
              evidence: undefined,
              input: {
                kind: "not-captured",
                id: `pending:${area}`,
                reason: "fixture",
              },
              gap: "fixture",
            },
      ),
    };
    assert.doesNotMatch(
      validateCatalogue(catalogue, { root: directory }).join("\n"),
      /does not declare test/,
    );
    writeFileSync(
      join(directory, "test.ts"),
      'it.each([])("keeps retry blocked after %s", () => {});\n',
    );
    assert.match(
      validateCatalogue(catalogue, { root: directory }).join("\n"),
      /does not declare test "keeps retry blocked after %s"/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("later nested Vitest parameter tables retain literal metacharacters without accepting helper calls", () => {
  const directory = mkdtempSync(
    join(tmpdir(), "dakia-mail-safety-later-each-test-"),
  );
  try {
    writeFileSync(
      join(directory, "test.ts"),
      [
        'it.each([{ value: "first" }].flatMap((value) => [value]))(',
        '  "earlier $field boundary",',
        "  () => {},",
        ");",
        'helper.it.each([["forged"]])("forged $field boundary", () => {});',
        '// it.each([["comment"]])("comment $field boundary", () => {});',
        'it.each([{ value: "later" }].flatMap((value) => [value]))(',
        '  "later $field boundary",',
        "  () => {},",
        ");",
        "",
      ].join("\n"),
    );
    const scenario = {
      id: "MS-A-001",
      area: "connection",
      priority: "P1",
      title: "fixture",
      standards: [
        {
          id: "fixture",
          status: "partial",
          requirement: "fixture",
          profile: "fixture",
          rationale: "fixture",
        },
      ],
      lanes: ["pull-request"],
      entrypoints: ["fixture"],
      status: "implemented",
      evidence: [{ path: "test.ts", symbol: "later $field boundary" }],
      input: { kind: "source-test", id: "test.ts#later $field boundary" },
      oracles: {
        remote: {
          status: "not-applicable",
          expectation: "fixture",
          reason: "fixture",
        },
        local: { status: "expected", expectation: "fixture" },
        ui: {
          status: "not-applicable",
          expectation: "fixture",
          reason: "fixture",
        },
      },
    };
    const catalogue = {
      schemaVersion: 1,
      title: "fixture",
      lanes: ["pull-request"],
      scenarios: "ABCDEFGHIJKL".split("").map((area) =>
        area === "A"
          ? scenario
          : {
              ...scenario,
              id: `MS-${area}-001`,
              area,
              status: "gap",
              evidence: undefined,
              input: {
                kind: "not-captured",
                id: `pending:${area}`,
                reason: "fixture",
              },
              gap: "fixture",
            },
      ),
    };
    assert.doesNotMatch(
      validateCatalogue(catalogue, { root: directory }).join("\n"),
      /does not declare test/,
    );
    for (const symbol of [
      "forged $field boundary",
      "comment $field boundary",
      "missing $field boundary",
    ]) {
      const errors = validateCatalogue(
        {
          ...catalogue,
          scenarios: catalogue.scenarios.map((item) =>
            item.status === "implemented"
              ? { ...item, evidence: [{ path: "test.ts", symbol }] }
              : item,
          ),
        },
        { root: directory },
      ).join("\n");
      assert.ok(
        errors.includes(`does not declare test ${JSON.stringify(symbol)}`),
      );
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("configured Vitest tests still require the declared test name", () => {
  const directory = mkdtempSync(
    join(tmpdir(), "dakia-mail-safety-configured-test-"),
  );
  try {
    writeFileSync(
      join(directory, "test.ts"),
      'it.skipIf(skip)("configured boundary", () => {});\n',
    );
    const scenario = {
      id: "MS-A-001",
      area: "connection",
      priority: "P1",
      title: "fixture",
      standards: [{ id: "fixture", status: "partial" }],
      lanes: ["pull-request"],
      entrypoints: ["fixture"],
      status: "implemented",
      evidence: [{ path: "test.ts", symbol: "configured boundary" }],
    };
    const catalogue = {
      schemaVersion: 1,
      title: "fixture",
      lanes: ["pull-request"],
      scenarios: "ABCDEFGHIJKL".split("").map((area) =>
        area === "A"
          ? scenario
          : {
              ...scenario,
              id: `MS-${area}-001`,
              area,
              status: "gap",
              evidence: undefined,
              gap: "fixture",
            },
      ),
    };
    const declaredErrors = validateCatalogue(catalogue, {
      root: directory,
    }).join("\n");
    assert.doesNotMatch(
      declaredErrors,
      /does not declare test "configured boundary"/,
    );
    const missingErrors = validateCatalogue(
      {
        ...catalogue,
        scenarios: catalogue.scenarios.map((item) =>
          item.status === "implemented"
            ? {
                ...item,
                evidence: [{ path: "test.ts", symbol: "missing boundary" }],
              }
            : item,
        ),
      },
      { root: directory },
    ).join("\n");
    assert.match(missingErrors, /does not declare test "missing boundary"/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("every scenario requires a source input, three outcome oracles, and a standards rationale", () => {
  const catalogue = loadCatalogue(
    join(root, "testdata/mail-safety-catalogue.json"),
  );
  const scenario = catalogue.scenarios.find(({ id }) => id === "MS-C-003");
  assert.ok(scenario);
  const withoutTraceability = {
    ...catalogue,
    scenarios: catalogue.scenarios.map((item) =>
      item.id === scenario.id
        ? {
            ...item,
            input: undefined,
            oracles: {
              ...item.oracles,
              remote: { status: "not-applicable", expectation: "no request" },
            },
            standards: item.standards.map((standard) => ({
              ...standard,
              rationale: "",
            })),
          }
        : item,
    ),
  };
  const errors = validateCatalogue(withoutTraceability, { root }).join("\n");
  assert.match(errors, /needs a traceable fixture or source input identifier/);
  assert.match(errors, /remote not-applicable outcome needs a reason/);
  assert.match(errors, /invalid standards requirement\/profile reference/);
});
