import { emit } from "@tauri-apps/api/event";
import type { MailCatalogueUpdated } from "./types";

const metricBufferLimit = 100;

export type MailPublicationMetric = {
  schemaVersion: 1;
  kind:
    | "catalogue_event_to_visible_row"
    | "account_connection_to_first_visible_row";
  accountId: string;
  revision?: number;
  startedAtUnixMs: number;
  visibleAtUnixMs: number;
  durationMs: number;
};

type PendingMetric = {
  accountId: string;
  revision?: number;
  startedAtUnixMs: number;
  startedAtMonotonicMs: number;
};

type Clock = {
  monotonicNow: () => number;
  unixNow: () => number;
};

type MetricSink = (metric: MailPublicationMetric) => void;

type Options = {
  enabled?: boolean;
  clock?: Clock;
  sink?: MetricSink;
};

declare global {
  interface Window {
    /** Development-only acceptance records. Never populated in production. */
    __DAKIA_ACCEPTANCE_METRICS__?: MailPublicationMetric[];
  }
}

function defaultClock(): Clock {
  return {
    monotonicNow: () => globalThis.performance?.now?.() ?? Date.now(),
    unixNow: () => Date.now(),
  };
}

function defaultSink(metric: MailPublicationMetric) {
  if (typeof window === "undefined") return;
  const records = (window.__DAKIA_ACCEPTANCE_METRICS__ ??= []);
  records.push(metric);
  if (records.length > metricBufferLimit) {
    records.splice(0, records.length - metricBufferLimit);
  }
  window.dispatchEvent(
    new CustomEvent("dakia:mail-publication-metric", { detail: metric }),
  );
  // The acceptance runner reads the debug Tauri process output. This is
  // emitted only by the opt-in development tracker below, never telemetry.
  if ("__TAURI_INTERNALS__" in window) {
    void emit("dakia:mail-publication-metric", metric).catch(() => undefined);
  }
}

function metricKey(kind: MailPublicationMetric["kind"], accountId: string) {
  return `${kind}:${accountId}`;
}

/**
 * Tracks only the part of synchronization that is observable in the rendered
 * list. Query completion is deliberately not a metric publication point.
 */
export function createMailPublicationMetrics({
  enabled = false,
  clock = defaultClock(),
  sink = defaultSink,
}: Options = {}) {
  const catalogue = new Map<string, PendingMetric>();
  const connections = new Map<string, PendingMetric>();
  const committed = new Map<
    string,
    { kind: MailPublicationMetric["kind"]; pending: PendingMetric }
  >();

  const start = (accountId: string, revision?: number): PendingMetric => ({
    accountId,
    revision,
    startedAtUnixMs: clock.unixNow(),
    startedAtMonotonicMs: clock.monotonicNow(),
  });

  const arm = (kind: MailPublicationMetric["kind"], pending: PendingMetric) => {
    committed.set(metricKey(kind, pending.accountId), { kind, pending });
  };

  return {
    beginCatalogue(update: MailCatalogueUpdated) {
      if (!enabled) return;
      // A burst of committed revisions triggers one guarded reload. Keep only
      // the most recent revision so the acceptance result matches that reload.
      catalogue.set(update.accountId, start(update.accountId, update.revision));
    },
    beginAccountConnection(accountId: string) {
      if (!enabled) return;
      connections.set(accountId, start(accountId));
    },
    /**
     * Called when a guarded query has scheduled a state update containing
     * rows. A later layout effect confirms the row made it into React's DOM.
     */
    armVisibleRows(
      visibleAccountIds: Iterable<string>,
      catalogueReloadAccountIds: Iterable<string> = [],
    ) {
      if (!enabled) return;
      const visible = new Set(visibleAccountIds);
      for (const accountId of catalogueReloadAccountIds) {
        const pending = catalogue.get(accountId);
        if (!pending) continue;
        catalogue.delete(accountId);
        if (visible.has(accountId)) {
          arm("catalogue_event_to_visible_row", pending);
        }
      }
      for (const [accountId, pending] of connections) {
        if (!visible.has(accountId)) continue;
        connections.delete(accountId);
        arm("account_connection_to_first_visible_row", pending);
      }
    },
    /** Called from a layout effect after React has committed mail-item nodes. */
    publishCommittedRows(visibleAccountIds: Iterable<string>) {
      if (!enabled) return;
      const visible = new Set(visibleAccountIds);
      for (const [key, entry] of committed) {
        if (!visible.has(entry.pending.accountId)) continue;
        const visibleAtMonotonicMs = clock.monotonicNow();
        sink({
          schemaVersion: 1,
          kind: entry.kind,
          accountId: entry.pending.accountId,
          ...(entry.pending.revision === undefined
            ? {}
            : { revision: entry.pending.revision }),
          startedAtUnixMs: entry.pending.startedAtUnixMs,
          visibleAtUnixMs: clock.unixNow(),
          durationMs: Math.max(
            0,
            Math.round(
              (visibleAtMonotonicMs - entry.pending.startedAtMonotonicMs) * 10,
            ) / 10,
          ),
        });
        committed.delete(key);
      }
    },
  };
}

export const mailPublicationMetrics = createMailPublicationMetrics({
  enabled:
    import.meta.env.DEV &&
    import.meta.env.VITE_DAKIA_ACCEPTANCE_METRICS === "1",
});
