import assert from "node:assert/strict";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import test from "node:test";
import { delimiter, join } from "node:path";

const root = new URL("..", import.meta.url).pathname;

test("fuzz runner uses a disposable corpus and preserves failed artifacts", async () => {
  const script = await readFile(
    join(root, "scripts/run-mail-safety-fuzz.sh"),
    "utf8",
  );
  assert.match(script, /mktemp -d/);
  assert.match(
    script,
    /cp "\$repo_root\/fuzz\/Cargo\.toml" "\$repo_root\/fuzz\/Cargo\.lock" "\$fuzz_root\/"/,
  );
  assert.match(
    script,
    /cp -R "\$repo_root\/fuzz\/fuzz_targets" "\$fuzz_root\/fuzz_targets"/,
  );
  assert.match(script, /vendor\/mail-parser/);
  assert.match(
    script,
    /s#path = \\"\\\.\\\.\/vendor\/lettre\\"#path = \\"\$repo_root\/vendor\/lettre\\"#/,
  );
  assert.doesNotMatch(script, /cp -R "\$repo_root\/fuzz\/\."/);
  assert.match(
    script,
    /cargo metadata --locked --offline --manifest-path "\$fuzz_root\/Cargo\.toml"/,
  );
  assert.match(script, /lock_before="\$fuzz_root\/Cargo\.lock\.before"/);
  assert.match(script, /cmp -s "\$lock_before" "\$fuzz_root\/Cargo\.lock"/);
  assert.match(script, /DAKIA_CARGO_FUZZ_BIN/);
  assert.match(
    script,
    /RUSTUP_TOOLCHAIN=nightly "\$cargo_fuzz_bin" run --fuzz-dir "\$fuzz_root"/,
  );
  assert.match(script, /DAKIA_FUZZ_TARGET_DIR/);
  assert.match(
    script,
    /ort_lib_location="\$\{ORT_LIB_LOCATION:-\$repo_root\/apps\/desktop\/src-tauri\/frameworks\}"/,
  );
  assert.match(script, /CARGO_TARGET_DIR="\$fuzz_target_dir"/);
  assert.match(
    script,
    /CARGO_NET_OFFLINE=true CARGO_TARGET_DIR="\$fuzz_target_dir" ORT_LIB_LOCATION="\$ort_lib_location" ORT_PREFER_DYNAMIC_LINK=1 DYLD_FALLBACK_LIBRARY_PATH=/,
  );
  assert.match(script, /cargo-fuzz 0\.13\.2 has no `--locked` option/);
  assert.match(script, /target_log="\$fuzz_root\/\$target\.log"/);
  assert.match(script, /tail -n 20 "\$target_log"/);
  assert.match(
    script,
    /Fuzzing did not complete\. Preserved crash artifacts and corpus at:/,
  );
  assert.match(script, /completed=true/);
  assert.match(
    script,
    /node "\$repo_root\/scripts\/seed-mail-safety-fuzz\.mjs"/,
  );
  assert.match(script, /-max_total_time="\$duration_seconds"/);
  assert.match(script, /-timeout=10/);
  assert.match(script, /-rss_limit_mb=512/);
  assert.match(script, /-max_len=1048576/);
  assert.doesNotMatch(script, /rm -rf "\$repo_root\/fuzz\/corpus"/);
});

const defaultAsanOptions =
  "quarantine_size_mb=64:thread_local_quarantine_size_kb=64";

async function writeExecutable(path, source) {
  await writeFile(path, source, { mode: 0o700 });
  await chmod(path, 0o700);
}

async function runWithFakeFuzzEnvironment({ asanOptions }) {
  const temporary = await mkdtemp(join(tmpdir(), "dakia-fuzz-runner-test-"));
  const bin = join(temporary, "bin");
  const asanLog = join(temporary, "asan-options.log");
  const fuzzTarget = join(temporary, "target");
  await mkdir(bin);
  const fakeCargoFuzz = join(bin, "cargo-fuzz");
  await Promise.all([
    writeExecutable(join(bin, "cargo"), "#!/bin/sh\nexit 0\n"),
    writeExecutable(join(bin, "rustup"), "#!/bin/sh\nexit 0\n"),
    writeExecutable(join(bin, "node"), "#!/bin/sh\nexit 0\n"),
    writeExecutable(
      fakeCargoFuzz,
      '#!/bin/sh\nprintf \'%s\\n\' "$ASAN_OPTIONS" >> "$DAKIA_TEST_ASAN_LOG"\nprintf \'%s\\n\' "$*" >> "$DAKIA_TEST_FUZZ_ARGS_LOG"\n',
    ),
  ]);

  const environment = {
    ...process.env,
    PATH: `${bin}${delimiter}${process.env.PATH}`,
    TMPDIR: temporary,
    DAKIA_CARGO_FUZZ_BIN: fakeCargoFuzz,
    DAKIA_FUZZ_TARGET_DIR: fuzzTarget,
    DAKIA_TEST_ASAN_LOG: asanLog,
    DAKIA_TEST_FUZZ_ARGS_LOG: join(temporary, "fuzz-args.log"),
  };
  if (asanOptions === undefined) delete environment.ASAN_OPTIONS;
  else environment.ASAN_OPTIONS = asanOptions;

  try {
    const result = spawnSync(
      "bash",
      [join(root, "scripts/run-mail-safety-fuzz.sh"), "1"],
      {
        cwd: root,
        encoding: "utf8",
        env: environment,
      },
    );
    assert.equal(result.status, 0, result.stderr || result.stdout);
    return {
      asanValues: (await readFile(asanLog, "utf8")).trimEnd().split("\n"),
      commandLines: (
        await readFile(environment.DAKIA_TEST_FUZZ_ARGS_LOG, "utf8")
      )
        .trimEnd()
        .split("\n"),
    };
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}

test("fuzz runner defaults only an unset ASAN_OPTIONS without disabling checks", async () => {
  const { asanValues, commandLines } = await runWithFakeFuzzEnvironment({});
  assert.deepEqual(asanValues, [defaultAsanOptions, defaultAsanOptions]);
  for (const value of asanValues) {
    assert.doesNotMatch(
      value,
      /(?:detect_odr_violation|detect_leaks|halt_on_error|abort_on_error)=/,
    );
  }
  assert.equal(commandLines.length, 2);
  for (const command of commandLines) {
    assert.match(command, /-max_total_time=1/);
    assert.match(command, /-timeout=10/);
    assert.match(command, /-rss_limit_mb=512/);
    assert.match(command, /-max_len=1048576/);
  }
});

test("fuzz runner preserves explicit ASAN_OPTIONS verbatim", async () => {
  const explicit = "detect_leaks=1:halt_on_error=1:allocator_may_return_null=0";
  const { asanValues } = await runWithFakeFuzzEnvironment({
    asanOptions: explicit,
  });
  assert.deepEqual(asanValues, [explicit, explicit]);
  assert.ok(
    asanValues.every((value) => !value.includes("quarantine_size_mb=64")),
  );
});

test("fuzz runner applies its bounded default when ASAN_OPTIONS is empty", async () => {
  const { asanValues } = await runWithFakeFuzzEnvironment({ asanOptions: "" });
  assert.deepEqual(asanValues, [defaultAsanOptions, defaultAsanOptions]);
});
