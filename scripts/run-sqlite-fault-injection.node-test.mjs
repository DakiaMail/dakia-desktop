import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const runner = join(root, "scripts/run-sqlite-fault-injection.sh");

async function fakeTools(directory) {
  const cargo = join(directory, "fake-cargo");
  const uname = join(directory, "uname");
  await writeFile(
    cargo,
    `#!/bin/sh
{
  printf 'target=%s\\n' "\${CARGO_TARGET_DIR:-}"
  printf 'offline=%s\\n' "\${CARGO_NET_OFFLINE:-}"
  printf 'pkg_config=%s\\n' "\${LIBSQLITE3_SYS_USE_PKG_CONFIG:-}"
  printf 'sqlite_flags=%s\\n' "\${LIBSQLITE3_FLAGS:-}"
  printf 'ort_location=%s\\n' "\${ORT_LIB_LOCATION:-}"
  printf 'ort_dynamic=%s\\n' "\${ORT_PREFER_DYNAMIC_LINK:-}"
  printf 'arg=%s\\n' "$@"
} > "\${DAKIA_SQLITE_FAULT_CAPTURE:?}"
exit "\${DAKIA_FAKE_CARGO_EXIT:-0}"
`,
  );
  await writeFile(uname, "#!/bin/sh\nprintf 'Linux\\n'\n");
  await Promise.all([chmod(cargo, 0o755), chmod(uname, 0o755)]);
  return cargo;
}

test("SQLite fault runner isolates its build and enables the exact manual harness", async () => {
  const directory = await mkdtemp(join(tmpdir(), "dakia-sqlite-fault-runner-"));
  const cargo = await fakeTools(directory);
  const capture = join(directory, "capture.txt");
  const target = join(directory, "target");
  const result = spawnSync("bash", [runner], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${directory}:${process.env.PATH}`,
      DAKIA_SQLITE_FAULT_CARGO_BIN: cargo,
      DAKIA_SQLITE_FAULT_CAPTURE: capture,
      DAKIA_SQLITE_FAULT_TARGET_DIR: target,
      ORT_LIB_LOCATION: join(directory, "ort"),
    },
  });
  assert.equal(result.status, 0, result.stderr);
  const invocation = await readFile(capture, "utf8");
  assert.match(invocation, new RegExp(`target=${target.replaceAll("/", "\\/")}`));
  assert.match(invocation, /offline=true/);
  assert.match(invocation, /pkg_config=0/);
  assert.match(
    invocation,
    /sqlite_flags=SQLITE_TEST -USQLITE_ENABLE_FTS3 -USQLITE_ENABLE_FTS3_PARENTHESIS/,
  );
  assert.match(invocation, /ort_dynamic=1/);
  assert.match(invocation, /arg=--locked/);
  assert.match(invocation, /arg=--offline/);
  assert.match(invocation, /arg=--features/);
  assert.match(invocation, /arg=sqlite-fault-injection/);
  assert.match(invocation, /arg=--test/);
  assert.match(invocation, /arg=sqlite_fault_injection/);
  assert.match(invocation, /^arg=--exact$/m);
  assert.match(invocation, /^arg=sqlite_write_fsync_and_torn_wal_preserve_committed_state$/m);
});

test("SQLite fault runner rejects a target directory inside the checkout", async () => {
  const result = spawnSync("bash", [runner], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      DAKIA_SQLITE_FAULT_TARGET_DIR: join(root, "target/sqlite-fault"),
    },
  });
  assert.equal(result.status, 64);
  assert.match(result.stderr, /must be outside the checkout/);
});
