import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workflow = readFileSync(
  resolve(root, ".github/workflows/production-release.yml"),
  "utf8",
);
const classifierDirectory = resolve(
  root,
  "apps/desktop/src-tauri/resources/email-classifier-v2",
);
const classifierManifest = JSON.parse(
  readFileSync(resolve(classifierDirectory, "MANIFEST.json"), "utf8"),
);
const gitAttributes = readFileSync(resolve(root, ".gitattributes"), "utf8");
const pullRequestWorkflow = readFileSync(
  resolve(root, ".github/workflows/pull-request-validation.yml"),
  "utf8",
);
const coreManifest = readFileSync(
  resolve(root, "crates/dakia-core/Cargo.toml"),
  "utf8",
);

function jobCondition(job) {
  const condition = job.match(
    /if:\s*>-\s*\n([\s\S]*?)\n\s+(?:runs-on|strategy):/,
  )?.[1];
  assert.ok(condition, "job condition is missing");
  return condition.trim().replace(/\s+/g, " ");
}

test("release builds run after the optional preparation job is skipped", () => {
  const buildJob = workflow.match(/\n  build:\n([\s\S]*?)\n  publish:\n/)?.[1];
  assert.ok(buildJob, "production release build job is missing");
  assert.match(buildJob, /if:\s*>-\s*\n\s+always\(\)/);
  assert.match(buildJob, /needs\.decide\.result == 'success'/);
  assert.match(buildJob, /needs\.validate\.result == 'success'/);
  assert.match(buildJob, /needs\.decide\.outputs\.should_release == 'true'/);
});

test("release publication runs after the optional preparation job is skipped", () => {
  const publishJob = workflow.match(/\n  publish:\n([\s\S]*?)\n  verify-public:\n/)?.[1];
  assert.ok(publishJob, "production release publish job is missing");
  assert.match(publishJob, /needs: \[decide, validate, build\]/);
  assert.equal(
    jobCondition(publishJob),
    "always() && !cancelled() && needs.decide.result == 'success' && needs.validate.result == 'success' && needs.build.result == 'success' && needs.decide.outputs.should_release == 'true'",
  );

  const verifyJob = workflow.match(/\n  verify-public:\n([\s\S]*)$/)?.[1];
  assert.ok(verifyJob, "production release verification job is missing");
  assert.match(verifyJob, /needs: \[decide, validate, publish\]/);
  assert.equal(
    jobCondition(verifyJob),
    "always() && !cancelled() && needs.decide.result == 'success' && needs.validate.result == 'success' && needs.publish.result == 'success' && needs.decide.outputs.should_release == 'true'",
  );
});

test("normalizes Windows runner paths before extracting the compiler cache", () => {
  assert.match(
    workflow,
    /runner_temp="\$RUNNER_TEMP"\n\s+if \[\[ "\$\{\{ matrix\.platform \}\}" == windows \]\]; then\n\s+runner_temp="\$\(cygpath -u "\$runner_temp"\)"\n\s+fi/,
  );
  assert.match(
    workflow,
    /archive="\$runner_temp\/\$\{\{ matrix\.sccache_archive \}\}"/,
  );
  assert.match(workflow, /install_dir="\$runner_temp\/sccache-bin"/);
  assert.doesNotMatch(
    workflow,
    /archive="\$RUNNER_TEMP\/\$\{\{ matrix\.sccache_archive \}\}"/,
  );
});
test("preserves the exact bytes of every pinned classifier asset", () => {
  const binaryPaths = new Set(
    gitAttributes
      .split("\n")
      .filter((line) => line.endsWith(" -text"))
      .map((line) => line.split(" ", 1)[0]),
  );

  for (const file of Object.keys(classifierManifest.files)) {
    assert.ok(
      binaryPaths.has(
        `/apps/desktop/src-tauri/resources/email-classifier-v2/${file}`,
      ),
      `${file} must be exempt from checkout line-ending conversion`,
    );
  }
});

test("keeps the tokenizer C++ training accelerator out of release builds", () => {
  assert.match(
    coreManifest,
    /^tokenizers = \{ version = "=0\.22\.2", default-features = false, features = \["onig"\] \}$/m,
  );
  assert.match(
    pullRequestWorkflow,
    /cargo tree --locked --target x86_64-pc-windows-msvc -p dakia-cli -e features -i esaxx-rs/,
  );
  assert.match(
    pullRequestWorkflow,
    /grep -Fq 'esaxx-rs feature "cpp"'/,
  );
});
