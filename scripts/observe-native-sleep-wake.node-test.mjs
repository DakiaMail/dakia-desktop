import assert from "node:assert/strict";
import test from "node:test";
import {
  countMarker,
  parsePowerCycle,
  retryEvidence,
} from "./observe-native-sleep-wake.mjs";

test("counts only complete realtime evidence markers", () => {
  assert.equal(
    countMarker(
      "new mail headers persisted\nnoise\nnew mail headers persisted\n",
      "new mail headers persisted",
    ),
    2,
  );
  assert.equal(
    countMarker("new mail headers persist", "new mail headers persisted"),
    0,
  );
});

test("requires a matching macOS sleep and wake pair of sufficient duration", () => {
  const log = [
    "2026-10-17 20:00:00 +0300 Sleep Entering Sleep state due to 'Software Sleep'",
    "2026-10-17 20:00:14 +0300 Wake Wake from Normal Sleep due to EC.LidOpen",
    "2026-10-17 20:01:00 +0300 Sleep Entering Sleep state due to 'Software Sleep'",
    "2026-10-17 20:01:16 +0300 Wake Wake from Normal Sleep due to EC.LidOpen",
  ].join("\n");
  assert.equal(
    parsePowerCycle(log, Date.parse("2026-10-17T20:00:30+03:00"))
      .durationMilliseconds,
    16_000,
  );
  assert.equal(
    parsePowerCycle(log, Date.parse("2026-10-17T20:01:20+03:00")),
    null,
  );
});

test("retry evidence rejects storms and accepts bounded recovery", () => {
  assert.deepEqual(retryEvidence(3, 7, 8_000), {
    retryDelta: 4,
    allowedRetries: 8,
    bounded: true,
  });
  assert.deepEqual(retryEvidence(3, 14, 120_000), {
    retryDelta: 11,
    allowedRetries: 10,
    bounded: false,
  });
  assert.equal(retryEvidence(5, 4, 1_000).bounded, false);
});
