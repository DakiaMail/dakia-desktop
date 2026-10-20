import { describe, expect, it, vi } from "vitest";
import { createMailPublicationMetrics } from "./mailPublicationMetrics";

function testMetrics() {
  let monotonic = 50;
  let unix = 1_700_000_000_000;
  const sink = vi.fn();
  const metrics = createMailPublicationMetrics({
    enabled: true,
    clock: {
      monotonicNow: () => monotonic,
      unixNow: () => unix,
    },
    sink,
  });
  return {
    metrics,
    sink,
    advance(milliseconds: number) {
      monotonic += milliseconds;
      unix += milliseconds;
    },
  };
}

describe("mail publication metrics", () => {
  it("coalesces a catalogue burst to its latest committed revision", () => {
    const { metrics, sink, advance } = testMetrics();
    metrics.beginCatalogue({ accountId: "account-a", revision: 8 });
    advance(4);
    metrics.beginCatalogue({ accountId: "account-a", revision: 9 });

    advance(12);
    metrics.armVisibleRows(["account-a"], ["account-a"]);
    advance(3);
    metrics.publishCommittedRows(["account-a"]);

    expect(sink).toHaveBeenCalledOnce();
    expect(sink).toHaveBeenCalledWith({
      schemaVersion: 1,
      kind: "catalogue_event_to_visible_row",
      accountId: "account-a",
      revision: 9,
      startedAtUnixMs: 1_700_000_000_004,
      visibleAtUnixMs: 1_700_000_000_019,
      durationMs: 15,
    });
  });

  it("does not publish an account's metric from another account's rows", () => {
    const { metrics, sink, advance } = testMetrics();
    metrics.beginCatalogue({ accountId: "account-a", revision: 4 });
    metrics.beginAccountConnection("account-b");

    advance(10);
    metrics.armVisibleRows(["account-b"], ["account-a"]);
    metrics.publishCommittedRows(["account-b"]);

    expect(sink).toHaveBeenCalledOnce();
    expect(sink).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "account_connection_to_first_visible_row",
        accountId: "account-b",
      }),
    );

    advance(5);
    metrics.armVisibleRows(["account-a"], ["account-a"]);
    metrics.publishCommittedRows(["account-a"]);
    expect(sink).toHaveBeenCalledTimes(1);
  });

  it("does nothing unless the development metric is explicitly enabled", () => {
    const sink = vi.fn();
    const metrics = createMailPublicationMetrics({ sink });
    metrics.beginCatalogue({ accountId: "account-a", revision: 1 });
    metrics.armVisibleRows(["account-a"], ["account-a"]);
    metrics.publishCommittedRows(["account-a"]);
    expect(sink).not.toHaveBeenCalled();
  });
});
