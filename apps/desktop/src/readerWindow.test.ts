import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReaderWindowSeed } from "./readerWindow";

const eventMocks = vi.hoisted(() => ({ emitTo: vi.fn(), listen: vi.fn() }));
const coreMocks = vi.hoisted(() => ({ invoke: vi.fn() }));
const windowMocks = vi.hoisted(() => ({ getCurrentWindow: vi.fn() }));
const webviewMocks = vi.hoisted(() => ({
  getAllWebviewWindows: vi.fn(),
  WebviewWindow: vi.fn(),
}));
const feedbackMocks = vi.hoisted(() => ({ showNativeMessage: vi.fn() }));
const readerSeeds = new Map<string, string>();
const readerSeedStorage = {
  getItem: (key: string) => readerSeeds.get(key) ?? null,
  setItem: (key: string, value: string) => readerSeeds.set(key, value),
  removeItem: (key: string) => readerSeeds.delete(key),
  clear: () => readerSeeds.clear(),
};

vi.mock("@tauri-apps/api/event", () => eventMocks);
vi.mock("@tauri-apps/api/core", () => coreMocks);
vi.mock("@tauri-apps/api/window", () => windowMocks);
vi.mock("@tauri-apps/api/webviewWindow", () => webviewMocks);
vi.mock("./nativeFeedback", () => feedbackMocks);

function installIndexedDb({ holdFirstPut = false } = {}) {
  const records = new Map<string, unknown>();
  const events = { puts: 0, deletes: 0 };
  let releaseFirstPut: (() => void) | undefined;
  const firstPut = new Promise<void>((resolve) => {
    releaseFirstPut = resolve;
  });
  const database = {
    objectStoreNames: { contains: () => true },
    close: vi.fn(),
    createObjectStore: vi.fn(),
    transaction: vi.fn(() => {
      const transaction: {
        oncomplete?: () => void;
        onerror?: () => void;
        onabort?: () => void;
        error?: DOMException | null;
        objectStore: () => {
          get: (key: string) => IDBRequest;
          put: (value: unknown, key: string) => void;
          delete: (key: string) => void;
        };
      } = {
        objectStore: () => ({
          get: (key) => {
            const request: Partial<IDBRequest> = {};
            queueMicrotask(() => {
              Object.assign(request, { result: records.get(key) });
              (request.onsuccess as ((event: Event) => void) | null)?.(
                new Event("success"),
              );
            });
            return request as IDBRequest;
          },
          put: (value, key) => {
            events.puts += 1;
            const complete = () => {
              records.set(key, value);
              transaction.oncomplete?.();
            };
            if (holdFirstPut && events.puts === 1) void firstPut.then(complete);
            else queueMicrotask(complete);
          },
          delete: (key) => {
            events.deletes += 1;
            queueMicrotask(() => {
              records.delete(key);
              transaction.oncomplete?.();
            });
          },
        }),
      };
      return transaction as unknown as IDBTransaction;
    }),
  };
  Object.defineProperty(globalThis, "indexedDB", {
    configurable: true,
    value: {
      open: vi.fn(() => {
        const request: Partial<IDBOpenDBRequest> = {
          result: database as unknown as IDBDatabase,
        };
        queueMicrotask(() => {
          (request.onsuccess as ((event: Event) => void) | null)?.(
            new Event("success"),
          );
        });
        return request as IDBOpenDBRequest;
      }),
    },
  });
  return { events, records, releaseFirstPut: () => releaseFirstPut?.() };
}

describe("reader window infrastructure", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    eventMocks.listen.mockResolvedValue(() => undefined);
    readerSeedStorage.clear();
    window.sessionStorage.clear();
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: readerSeedStorage,
    });
    window.history.replaceState({}, "", "/");
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    Object.defineProperty(window, "open", {
      configurable: true,
      writable: true,
      value: vi.fn(() => ({}) as Window),
    });
  });

  it("uses a deterministic label-safe hash without identifiers", async () => {
    const { readerWindowLabel } = await import("./readerWindow");
    const target = {
      accountId: "account/alex@example.com",
      threadId: "<conversations/123> with spaces",
    };

    const label = await readerWindowLabel(target);
    expect(label).toBe(await readerWindowLabel(target));
    expect(label).toMatch(/^reader-[a-f0-9]{32}$/);
    expect(label).not.toContain("alex");
  });

  it("uses an opaque localStorage token for browser reader targets", async () => {
    const { openReaderWindow } = await import("./readerWindow");

    await openReaderWindow({
      target: {
        accountId: "account-1",
        threadId: "thread-secret",
        localMessageId: "message-secret",
      },
      focusedMessageId: "message-secret",
    });

    expect(window.open).toHaveBeenCalledOnce();
    const [url, label] = vi.mocked(window.open).mock.calls[0];
    const params = new URL(String(url), "http://localhost").searchParams;
    const token = params.get("seedKey");
    expect(params.get("view")).toBe("reader");
    expect(token).toBeTruthy();
    expect(String(url)).not.toContain("thread-secret");
    expect(String(label)).toMatch(/^reader-[a-f0-9]{32}$/);
    expect(readerSeedStorage.getItem(`dakia.reader-seed.${token}`)).toContain(
      "thread-secret",
    );
  });

  it("consumes the handoff seed once and preserves it for window reloads", async () => {
    const token = "reader-seed";
    readerSeedStorage.setItem(
      `dakia.reader-seed.${token}`,
      JSON.stringify({
        target: { accountId: "account-1", threadId: "thread-1" },
        focusedMessageId: "message-3",
      }),
    );
    window.history.replaceState({}, "", `/?view=reader&seedKey=${token}`);
    const { readReaderSeed } = await import("./readerWindow");

    expect(readReaderSeed()).toEqual({
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-3",
    });
    expect(readReaderSeed()).toEqual({
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-3",
    });
  });

  it("focuses and retargets an existing conversation window", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    const existing = {
      label: "reader-9128d2de",
      show: vi.fn(),
      setFocus: vi.fn(),
    };
    webviewMocks.getAllWebviewWindows.mockResolvedValue([existing]);
    const { openReaderWindow, readerWindowLabel } =
      await import("./readerWindow");
    const seed = {
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-2",
    };
    existing.label = await readerWindowLabel(seed.target);

    await openReaderWindow(seed);

    expect(existing.show).toHaveBeenCalledOnce();
    expect(existing.setFocus).toHaveBeenCalledOnce();
    expect(eventMocks.emitTo).toHaveBeenCalledWith(
      existing.label,
      "reader-target",
      { label: existing.label, seed },
    );
    expect(webviewMocks.WebviewWindow).not.toHaveBeenCalled();
  });

  it("delivers the initial native Reader target only after its exact ready acknowledgement", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    webviewMocks.getAllWebviewWindows.mockResolvedValue([]);
    acknowledgeReaderReadyOnInvoke();
    const { openReaderWindow } = await import("./readerWindow");

    const seed = {
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-1",
    };
    await openReaderWindow(seed);

    expect(coreMocks.invoke).toHaveBeenCalledWith("open_reader_window", {
      label: expect.stringMatching(/^reader-[a-f0-9]{32}$/),
      seedKey: null,
      seedDbKey: expect.any(String),
    });
    const args = coreMocks.invoke.mock.calls[0][1];
    expect(eventMocks.emitTo).toHaveBeenLastCalledWith(
      args.label,
      "reader-target",
      { label: args.label, seedToken: args.seedDbKey, seed },
    );
    expect(readerSeeds.size).toBe(0);
  });

  it("serializes same-label opens so the latest target retargets without another seed", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    const seed = {
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-1",
    };
    const latest = { ...seed, focusedMessageId: "message-2" };
    const label = await (
      await import("./readerWindow")
    ).readerWindowLabel(seed.target);
    webviewMocks.getAllWebviewWindows
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([]);
    let readerReady:
      | ((event: { payload: { label: string; seedToken: string } }) => void)
      | undefined;
    eventMocks.listen.mockImplementation(async (event, handler) => {
      if (event === "reader-ready") readerReady = handler;
      return () => undefined;
    });
    let releaseFirstCreate: () => void = () => undefined;
    const firstCreate = new Promise<void>((resolve) => {
      releaseFirstCreate = resolve;
    });
    let releaseCorrectReady: () => void = () => undefined;
    const correctReady = new Promise<void>((resolve) => {
      releaseCorrectReady = resolve;
    });
    coreMocks.invoke.mockImplementationOnce(async (_command, args) => {
      await firstCreate;
      readerReady?.({
        payload: { label: args.label, seedToken: "stale-window-seed" },
      });
      await correctReady;
      readerReady?.({
        payload: { label: args.label, seedToken: args.seedDbKey },
      });
    });
    const { openReaderWindow } = await import("./readerWindow");

    const first = openReaderWindow(seed);
    await new Promise((resolve) => setTimeout(resolve, 0));
    const second = openReaderWindow(latest);
    await new Promise((resolve) => setTimeout(resolve, 0));
    releaseFirstCreate();
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(coreMocks.invoke).toHaveBeenCalledOnce();
    releaseCorrectReady();
    await Promise.all([first, second]);

    expect(coreMocks.invoke).toHaveBeenCalledOnce();
    expect(eventMocks.emitTo).toHaveBeenCalledWith(label, "reader-target", {
      label,
      seed: latest,
    });
  });

  it("rejects when native reader-window creation fails", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    webviewMocks.getAllWebviewWindows.mockResolvedValue([]);
    coreMocks.invoke.mockRejectedValueOnce("denied");
    const { openReaderWindow } = await import("./readerWindow");

    await expect(
      openReaderWindow({
        target: { accountId: "account-1", threadId: "thread-1" },
      }),
    ).rejects.toThrow("Could not open reader window: denied");
    expect(readerSeeds.size).toBe(0);
    expect(coreMocks.invoke).toHaveBeenCalledWith(
      "open_reader_window",
      expect.objectContaining({ seedKey: null, seedDbKey: expect.any(String) }),
    );
  });

  it("restores the newest Reader target from child session storage", async () => {
    const token = "ccbf706f-079f-4bf8-b7e0-2e7c39735bfe";
    const first = {
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-1",
    };
    const latest = {
      target: { accountId: "account-1", threadId: "thread-2" },
      focusedMessageId: "message-2",
    };
    window.history.replaceState({}, "", `/?view=reader&seedDbKey=${token}`);
    const { hasNativeReaderSeedHandoff, readReaderSeed, rememberReaderSeed } =
      await import("./readerWindow");

    rememberReaderSeed(first);
    expect(hasNativeReaderSeedHandoff()).toBe(false);
    expect(readReaderSeed()).toEqual(first);

    rememberReaderSeed(latest);
    expect(readReaderSeed()).toEqual(latest);
  });

  it("delivers a retarget even when session storage is unavailable", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const sessionWrite = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw new DOMException("storage denied", "SecurityError");
      });
    const seed = {
      target: { accountId: "account-1", threadId: "thread-1" },
      focusedMessageId: "message-1",
    };
    const { readerWindowLabel } = await import("./readerWindow");
    windowMocks.getCurrentWindow.mockReturnValue({
      label: await readerWindowLabel(seed.target),
    });
    const nativeDigest = crypto.subtle.digest.bind(crypto.subtle);
    let releaseDigest: () => void = () => undefined;
    const digestGate = new Promise<void>((resolve) => {
      releaseDigest = resolve;
    });
    const digestCheck = vi
      .spyOn(crypto.subtle, "digest")
      .mockImplementation(async (algorithm, data) => {
        await digestGate;
        return nativeDigest(algorithm, data);
      });
    let delivered: ReaderWindowSeed | undefined;
    eventMocks.listen.mockImplementationOnce(async (_event, handler) => {
      handler({
        payload: {
          label: windowMocks.getCurrentWindow().label,
          seed,
        },
      });
      return () => undefined;
    });
    const { onReaderTarget } = await import("./readerWindow");

    try {
      await onReaderTarget((seed) => {
        delivered = seed;
      });

      expect(delivered).toBeUndefined();
      expect(sessionWrite).not.toHaveBeenCalled();
      releaseDigest();
      await vi.waitFor(() => {
        expect(delivered).toMatchObject({ focusedMessageId: "message-1" });
      });
      expect(sessionWrite).toHaveBeenCalledOnce();
    } finally {
      releaseDigest();
      digestCheck.mockRestore();
      sessionWrite.mockRestore();
    }
  });

  it("rejects when an opaque reader seed cannot be stored", async () => {
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: {
        ...readerSeedStorage,
        setItem: () => {
          throw new Error("storage denied");
        },
      },
    });
    const { openReaderWindow } = await import("./readerWindow");

    await expect(
      openReaderWindow({
        target: { accountId: "account-1", threadId: "thread-1" },
      }),
    ).rejects.toThrow("Could not create reader window seed");
  });

  it("opens a native Reader without relying on opener IndexedDB", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    webviewMocks.getAllWebviewWindows.mockResolvedValue([]);
    acknowledgeReaderReadyOnInvoke();
    const { openReaderWindow } = await import("./readerWindow");
    await openReaderWindow({
      target: { accountId: "account-1", threadId: "thread-1" },
    });
    expect(coreMocks.invoke).toHaveBeenCalledOnce();
  });

  it("cleans up its listener when native Reader creation never becomes ready", async () => {
    vi.useFakeTimers();
    try {
      (
        window as Window & { __TAURI_INTERNALS__?: unknown }
      ).__TAURI_INTERNALS__ = {};
      webviewMocks.getAllWebviewWindows.mockResolvedValue([]);
      const dispose = vi.fn();
      eventMocks.listen.mockResolvedValue(dispose);
      coreMocks.invoke.mockReturnValue(new Promise<void>(() => undefined));
      const { openReaderWindow } = await import("./readerWindow");

      const opening = openReaderWindow({
        target: { accountId: "account-1", threadId: "thread-1" },
      });
      const outcome = opening.then(
        () => undefined,
        (error) => error,
      );
      await vi.advanceTimersByTimeAsync(0);
      await vi.advanceTimersByTimeAsync(5_000);

      expect(await outcome).toEqual(
        expect.objectContaining({
          message: expect.stringContaining("Could not open reader window"),
        }),
      );
      expect(dispose).toHaveBeenCalledOnce();
      expect(feedbackMocks.showNativeMessage).toHaveBeenCalledWith(
        "Could not open this message",
        "Try again.",
        "error",
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("rejects and removes its seed when a browser blocks the popup", async () => {
    vi.mocked(window.open).mockReturnValueOnce(null);
    const { openReaderWindow } = await import("./readerWindow");

    await expect(
      openReaderWindow({
        target: { accountId: "account-1", threadId: "thread-1" },
      }),
    ).rejects.toThrow("Could not open reader window");
    expect(readerSeeds.size).toBe(0);
  });
});

function acknowledgeReaderReadyOnInvoke() {
  let readerReady:
    | ((event: { payload: { label: string; seedToken: string } }) => void)
    | undefined;
  eventMocks.listen.mockImplementation(async (event, handler) => {
    if (event === "reader-ready") readerReady = handler;
    return () => undefined;
  });
  coreMocks.invoke.mockImplementation(async (_command, args) => {
    readerReady?.({
      payload: { label: args.label, seedToken: args.seedDbKey },
    });
  });
}
