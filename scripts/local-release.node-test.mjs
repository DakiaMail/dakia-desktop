import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const verifier = join(root, "scripts/verify-local-updater-evidence.sh");
const publisher = join(root, "scripts/publish-release-to-r2.sh");
const sha256 = (value) =>
  createHash("sha256").update(value).digest("hex");

function createEvidence() {
  const evidenceRoot = mkdtempSync(join(tmpdir(), "dakia-evidence-test-"));
  for (const arch of ["aarch64", "x86_64"]) {
    for (const mode of [
      "valid",
      "tampered-archive",
      "invalid-signature",
    ]) {
      const resultDir = join(evidenceRoot, arch, mode);
      mkdirSync(resultDir, { recursive: true });
      const profile = "accounts=1 messages=2 sha256=test-profile\n";
      const events =
        mode === "valid"
          ? [
              { event: "launched", detail: "0.2.7" },
              { event: "update-available", detail: "0.2.8" },
              { event: "downloaded", detail: "0.2.8" },
              { event: "installing", detail: "0.2.8" },
              { event: "launched", detail: "0.2.8" },
              { event: "completed", detail: "0.2.8" },
            ]
              .map(JSON.stringify)
              .join("\n") + "\n"
          : [
              { event: "launched", detail: "0.2.7" },
              { event: "signature-rejected", detail: mode },
            ]
              .map(JSON.stringify)
              .join("\n") + "\n";
      writeFileSync(join(resultDir, "evidence.jsonl"), events);
      writeFileSync(join(resultDir, "profile-before.txt"), profile);
      writeFileSync(join(resultDir, "profile-after.txt"), profile);
      writeFileSync(
        join(resultDir, "result.json"),
        JSON.stringify({
          schema: "dakia-local-updater-acceptance-v1",
          result: "passed",
          arch,
          mode,
          baseline_tag: "v0.2.7",
          target_tag: "v0.2.8",
          final_version: mode === "valid" ? "0.2.8" : "0.2.7",
          evidence_sha256: sha256(events),
          profile_sha256: sha256(profile),
        }),
      );
    }
  }
  return evidenceRoot;
}

test("accepts a complete two-architecture, three-mode evidence set", () => {
  const evidenceRoot = createEvidence();
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /6 passed, 0 waived/);
});

test("accepts an explicit rejection-test waiver without treating it as passed", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "tampered-archive");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "tampered-archive",
      target_tag: "v0.2.8",
      reason: "Release owner explicitly chose to skip this test.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /5 passed, 1 waived/);
});

test("accepts an explicitly risk-acknowledged install-and-restart waiver", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "valid");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "valid",
      target_tag: "v0.2.8",
      reason: "Release owner accepted deferred Intel upgrade verification.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
      risk_acknowledged: true,
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /5 passed, 1 waived/);
});

test("rejects an install-and-restart waiver without risk acknowledgement", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "valid");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "valid",
      target_tag: "v0.2.8",
      reason: "Missing explicit risk acknowledgement.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
});

test("rejects evidence changed after the result was recorded", () => {
  const evidenceRoot = createEvidence();
  const events = join(evidenceRoot, "x86_64", "valid", "evidence.jsonl");
  writeFileSync(events, readFileSync(events, "utf8") + '{"event":"tampered"}\n');
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
});

test("low-level publisher refuses direct production publication", () => {
  const result = spawnSync(publisher, ["v0.2.8", tmpdir()], {
    encoding: "utf8",
    env: {
      ...process.env,
      DAKIA_UPDATER_CHANNEL: "production",
    },
  });
  assert.notEqual(result.status, 0);
  assert.match(
    result.stderr,
    /Production publication must run through publish-local-production-release/,
  );
});
