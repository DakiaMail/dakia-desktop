import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

test("Gmail provider identity wrapper offline suite passes", () => {
  const result = spawnSync("python3", ["scripts/run-gmail-provider-identity-readonly.test.py"], {
    encoding: "utf8",
    env: { PATH: process.env.PATH },
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stderr, /Ran 9 tests/);
});
