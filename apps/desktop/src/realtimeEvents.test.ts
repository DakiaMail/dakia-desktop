import { beforeEach, describe, expect, it, vi } from "vitest";
import fixture from "../testdata/tauri-contracts/high-risk.json";
import type {
  MailArrival,
  MailHydrated,
  MailSummary,
  RealtimeSyncStatus,
} from "./types";

const listeners = vi.hoisted(() => {
  const events = new Map<
    string,
    Array<(event: { payload: unknown }) => void>
  >();
  const unlisten = vi.fn();
  return { events, unlisten };
});

vi.mock("@tauri-apps/api/event", () => ({
  emitTo: vi.fn(async () => undefined),
  listen: vi.fn(),
}));

import { listen } from "@tauri-apps/api/event";
import { subscribeRealtimeEvents } from "./realtimeEvents";

function registerImmediately(event: string, handler: (event: any) => void) {
  const handlers = listeners.events.get(event) ?? [];
  handlers.push(handler);
  listeners.events.set(event, handlers);
  return Promise.resolve(listeners.unlisten as () => void);
}

const message = {
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 42,
  thread_id: "thread-1",
  subject: "New mail",
  from_address: "sender@example.test",
  to_addresses: "recipient@example.test",
  received_at: "2026-10-17T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
} satisfies MailSummary;

function emit(event: string, payload: unknown) {
  for (const handler of listeners.events.get(event) ?? []) {
    handler({ payload });
  }
}

describe("main realtime event subscriptions", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    listeners.events.clear();
    vi.mocked(listen).mockImplementation(registerImmediately);
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  it("delivers native arrival, catalogue, hydration, and state events to their frontend owners", async () => {
    const handlers = {
      arrived: vi.fn(),
      changed: vi.fn(),
      hydrated: vi.fn(),
      syncState: vi.fn(),
    };
    await subscribeRealtimeEvents(handlers);

    const arrival: MailArrival = {
      eventId: "event-1",
      accountId: "account-1",
      messages: [message],
      detectedAt: "2026-10-17T10:00:01Z",
    };
    const hydrated: MailHydrated = {
      accountId: "account-1",
      messageId: message.id,
    };
    const status: RealtimeSyncStatus = {
      accountId: "account-1",
      state: "idle",
      retryAt: null,
      errorKind: null,
    };
    emit("mail-arrived", arrival);
    emit("mail-changed", { accountId: "account-1" });
    emit("mail-hydrated", hydrated);
    emit("mail-sync-state", status);

    expect(handlers.arrived).toHaveBeenCalledWith(arrival);
    expect(handlers.changed).toHaveBeenCalledWith("account-1");
    expect(handlers.hydrated).toHaveBeenCalledWith(hydrated);
    expect(handlers.syncState).toHaveBeenCalledWith(status);
  });

  it("removes every native subscription when the main frontend unmounts", async () => {
    const unlisten = await subscribeRealtimeEvents({
      arrived: vi.fn(),
      changed: vi.fn(),
      hydrated: vi.fn(),
      syncState: vi.fn(),
    });

    unlisten();

    expect(listeners.unlisten).toHaveBeenCalledTimes(4);
  });

  it("continues cleaning sibling listeners when one unlisten throws", async () => {
    const allUnlisteners = [
      vi.fn(() => {
        throw new Error("already closed");
      }),
      vi.fn(),
      vi.fn(),
      vi.fn(),
    ];
    const pendingUnlisteners = [...allUnlisteners];
    vi.mocked(listen).mockImplementation(async () =>
      pendingUnlisteners.shift()!,
    );
    const unlisten = await subscribeRealtimeEvents({
      arrived: vi.fn(),
      changed: vi.fn(),
      hydrated: vi.fn(),
      syncState: vi.fn(),
    });
    expect(() => unlisten()).not.toThrow();
    expect(allUnlisteners).toHaveLength(4);
    for (const listener of allUnlisteners)
      expect(listener).toHaveBeenCalledOnce();
  });

  it("rejects promptly with the original registration error and cleans successful siblings", async () => {
    const failure = new Error("mail-hydrated listener unavailable");
    const arrivedUnlisten = vi.fn();
    vi.mocked(listen).mockImplementation((event) => {
      if (event === "mail-arrived") return Promise.resolve(arrivedUnlisten);
      if (event === "mail-hydrated") return Promise.reject(failure);
      return new Promise(() => undefined);
    });

    await expect(
      subscribeRealtimeEvents({
        arrived: vi.fn(),
        changed: vi.fn(),
        hydrated: vi.fn(),
        syncState: vi.fn(),
      }),
    ).rejects.toBe(failure);

    expect(arrivedUnlisten).toHaveBeenCalledOnce();
  });

  it("self-cleans a listener that resolves after a sibling registration failed", async () => {
    const failure = new Error("mail-hydrated listener unavailable");
    const arrivedUnlisten = vi.fn();
    const lateChangedUnlisten = vi.fn();
    let resolveChanged: (unlisten: () => void) => void = () => undefined;
    vi.mocked(listen).mockImplementation((event) => {
      if (event === "mail-arrived") return Promise.resolve(arrivedUnlisten);
      if (event === "mail-hydrated") return Promise.reject(failure);
      if (event === "mail-changed") {
        return new Promise((resolve) => {
          resolveChanged = resolve;
        });
      }
      return new Promise(() => undefined);
    });

    await expect(
      subscribeRealtimeEvents({
        arrived: vi.fn(),
        changed: vi.fn(),
        hydrated: vi.fn(),
        syncState: vi.fn(),
      }),
    ).rejects.toBe(failure);
    resolveChanged(lateChangedUnlisten);
    await Promise.resolve();

    expect(arrivedUnlisten).toHaveBeenCalledOnce();
    expect(lateChangedUnlisten).toHaveBeenCalledOnce();
  });

  it("cancels successful and late registrations while a sibling is still pending", async () => {
    const controller = new AbortController();
    const arrivedUnlisten = vi.fn();
    const lateChangedUnlisten = vi.fn();
    let resolveChanged: (unlisten: () => void) => void = () => undefined;
    vi.mocked(listen).mockImplementation((event) => {
      if (event === "mail-arrived") return Promise.resolve(arrivedUnlisten);
      if (event === "mail-changed") {
        return new Promise((resolve) => {
          resolveChanged = resolve;
        });
      }
      return new Promise(() => undefined);
    });

    const subscription = subscribeRealtimeEvents(
      {
        arrived: vi.fn(),
        changed: vi.fn(),
        hydrated: vi.fn(),
        syncState: vi.fn(),
      },
      controller.signal,
    );
    await Promise.resolve();
    controller.abort();
    const unlisten = await subscription;

    expect(arrivedUnlisten).toHaveBeenCalledOnce();
    expect(() => unlisten()).not.toThrow();
    resolveChanged(lateChangedUnlisten);
    await Promise.resolve();
    expect(lateChangedUnlisten).toHaveBeenCalledOnce();
  });

  it("settles every registration when cancellation was already requested", async () => {
    const controller = new AbortController();
    controller.abort();
    const lateArrivedUnlisten = vi.fn();
    let resolveArrived: (unlisten: () => void) => void = () => undefined;
    vi.mocked(listen).mockImplementation((event) => {
      if (event === "mail-arrived") {
        return new Promise((resolve) => {
          resolveArrived = resolve;
        });
      }
      if (event === "mail-hydrated") {
        return Promise.reject(new Error("late registration rejection"));
      }
      return new Promise(() => undefined);
    });

    const unlisten = await subscribeRealtimeEvents(
      {
        arrived: vi.fn(),
        changed: vi.fn(),
        hydrated: vi.fn(),
        syncState: vi.fn(),
      },
      controller.signal,
    );
    expect(() => unlisten()).not.toThrow();
    resolveArrived(lateArrivedUnlisten);
    await Promise.resolve();
    await Promise.resolve();

    expect(lateArrivedUnlisten).toHaveBeenCalledOnce();
  });

  it("passes the shared late arrival unchanged through the native adapter to the App owner", async () => {
    const handlers = {
      arrived: vi.fn(),
      changed: vi.fn(),
      hydrated: vi.fn(),
      syncState: vi.fn(),
    };
    await subscribeRealtimeEvents(handlers);
    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    expect(removal.event).toBe("account-removed");
    expect(lateArrival.event).toBe("mail-arrived");

    emit(
      lateArrival.event,
      JSON.parse(JSON.stringify(lateArrival.payload)) as MailArrival,
    );

    expect(handlers.arrived).toHaveBeenCalledOnce();
    expect(handlers.arrived).toHaveBeenCalledWith(lateArrival.payload);
  });
});
