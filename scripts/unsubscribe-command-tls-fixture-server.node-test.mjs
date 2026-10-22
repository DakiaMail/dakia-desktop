import assert from "node:assert/strict";
import test from "node:test";

import { parseArguments } from "./unsubscribe-command-tls-fixture-server.mjs";

const args = [
  "--good-cert",
  "good.crt",
  "--good-key",
  "good.key",
  "--wrong-cert",
  "wrong.crt",
  "--wrong-key",
  "wrong.key",
  "--port-file",
  "ports.json",
  "--receipt",
  "receipt.json",
];

test("fictional unsubscribe TLS server requires each bounded input exactly once", () => {
  assert.deepEqual(parseArguments(args), {
    "--good-cert": "good.crt",
    "--good-key": "good.key",
    "--wrong-cert": "wrong.crt",
    "--wrong-key": "wrong.key",
    "--port-file": "ports.json",
    "--receipt": "receipt.json",
  });
  assert.throws(() => parseArguments(args.slice(0, -2)));
  assert.throws(() => parseArguments([...args, "--receipt", "other.json"]));
});
