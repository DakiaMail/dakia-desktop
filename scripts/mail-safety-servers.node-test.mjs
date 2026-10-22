import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { join } from "node:path";

const root = new URL("..", import.meta.url).pathname;

test("independent server harness is loopback-only, version-pinned, and tears down", async () => {
  const script = await readFile(join(root, "scripts/mail-safety-servers.sh"), "utf8");
  assert.match(script, /greenmail\/standalone:2\.1\.14/);
  assert.match(script, /dovecot\/dovecot:2\.4\.5/);
  assert.match(script, /-p 127\.0\.0\.1::3993/);
  assert.match(script, /-p 127\.0\.0\.1::3465/);
  assert.match(script, /-p 127\.0\.0\.1::31993/);
  assert.match(script, /-Dgreenmail\.hostname=0\.0\.0\.0/);
  assert.match(script, /greenmail\.tls\.keystore\.file=\/tls\/greenmail\.p12/);
  assert.match(script, /greenmail\.users\.login=email/);
  assert.match(script, /greenmail\.preload\.dir=\/tls\/preload/);
  assert.match(script, /subjectAltName=DNS:localhost,IP:127\.0\.0\.1/);
  assert.match(script, /trap cleanup EXIT/);
  assert.match(script, /docker rm --force/);
  assert.match(script, /logs after integration failure/);
  assert.match(script, /external_server_rejects_untrusted_tls/);
  assert.match(script, /PROVIDER_SMOKE_CONFIG/);
  assert.match(script, /Provider smoke failed; no remote mailbox mutation was requested\./);
  assert.match(script, /DAKIA_INDEPENDENT_CA_DER_PATH/);
  assert.match(script, /independent_server_tests::external_server_roundtrip/);
  assert.match(
    script,
    /independent_server_tests::external_dovecot_selective_mime_roundtrip/,
  );
  assert.match(script, /independent_server_tests::external_server_greenmail_roundtrip/);
  assert.match(script, /rawlog_dir = \/rawlog/);
  assert.match(script, /doveadm save/);
  assert.match(script, /verify-dovecot-selective-rawlog\.mjs/);
  assert.match(script, /Docker daemon is unavailable; independent-server integration was not run\./);
  assert.match(script, /wait_for_tls/);
  assert.match(script, /DAKIA_INDEPENDENT_SERVER_HOST=127\.0\.0\.1/);
  assert.match(script, /unauthenticated loopback handshake/);
  assert.doesNotMatch(script, /:latest(?:["'\s]|$)/);
});
