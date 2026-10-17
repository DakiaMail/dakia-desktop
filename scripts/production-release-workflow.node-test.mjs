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
  const publishJob = workflow.match(
    /\n  publish:\n([\s\S]*?)\n  verify-public:\n/,
  )?.[1];
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

test("prepares the agent runtime for the release matrix target", () => {
  const assetPreparation = workflow.match(
    /- name: Prepare packaged assets([\s\S]*?)- name: Build target-specific CLI sidecar/,
  )?.[1];
  assert.ok(assetPreparation, "packaged asset preparation step is missing");
  assert.match(
    assetPreparation,
    /TAURI_ENV_PLATFORM: \$\{\{ matrix\.platform \}\}/,
  );
  assert.match(assetPreparation, /TAURI_ENV_ARCH: \$\{\{ matrix\.arch \}\}/);
  assert.match(
    assetPreparation,
    /bash \.\/scripts\/prepare-desktop-assets\.sh/,
  );
});

test("writes canonical Windows checksums while hashing binary bytes", () => {
  const windowsAssets = workflow.match(
    /- name: Sign and assemble Windows release assets([\s\S]*?)- name: Verify macOS release asset set/,
  )?.[1];
  assert.ok(windowsAssets, "Windows release asset step is missing");
  assert.match(
    windowsAssets,
    /sha256sum --binary "\$artifact" \| awk '\{print \$1\}'/,
  );
  assert.match(windowsAssets, /printf '%s  %s\\n' "\$digest" "\$artifact"/);
  assert.doesNotMatch(
    windowsAssets,
    /sha256sum "\$expected" "\$expected\.sig" > SHA256SUMS\.txt/,
  );
});

test("verifies the installed Linux agent runtime without system Node", () => {
  const linuxAssets = workflow.match(
    /- name: Assemble and verify Linux release assets([\s\S]*?)- name: Sign and assemble Windows release assets/,
  )?.[1];
  assert.ok(linuxAssets, "Linux release asset step is missing");
  assert.match(linuxAssets, /usr\/lib\/dakia-desktop\/agent/);
  assert.match(linuxAssets, /prepare-agent-runtime\.mjs verify "\$agent"/);
  assert.match(linuxAssets, /test -x "\$agent\/runtime\/node"/);
  assert.match(linuxAssets, /PATH="\$empty_path" "\$cli"/);
  assert.match(linuxAssets, /--json agent sessions list/);
  assert.match(linuxAssets, /value\.sessions/);
});

test("extracts and verifies the installed Windows agent runtime", () => {
  const windowsAssets = workflow.match(
    /- name: Sign and assemble Windows release assets([\s\S]*?)- name: Verify macOS release asset set/,
  )?.[1];
  assert.ok(windowsAssets, "Windows release asset step is missing");
  assert.match(windowsAssets, /7z\.exe x/);
  assert.match(windowsAssets, /find "\$outer" -type f -name 'app-\*\.7z'/);
  assert.match(windowsAssets, /test -f "\$agent\/runtime\/node\.exe"/);
  assert.match(windowsAssets, /prepare-agent-runtime\.mjs verify "\$agent"/);
  assert.match(windowsAssets, /PATH="\$empty_path" "\$cli"/);
  assert.match(windowsAssets, /--json agent sessions list/);
  assert.match(windowsAssets, /value\.sessions/);
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
  assert.match(pullRequestWorkflow, /grep -Fq 'esaxx-rs feature "cpp"'/);
});
