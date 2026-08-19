import { beforeEach, describe, expect, it, vi } from "vitest";

const eventMocks = vi.hoisted(() => ({ emitTo: vi.fn(), listen: vi.fn() }));
const windowMocks = vi.hoisted(() => ({ getCurrentWindow: vi.fn() }));
const webviewMocks = vi.hoisted(() => ({
  getAllWebviewWindows: vi.fn(),
  WebviewWindow: vi.fn(),
}));
const readerSeeds = new Map<string, string>();
const readerSeedStorage = {
  getItem: (key: string) => readerSeeds.get(key) ?? null,
  setItem: (key: string, value: string) => readerSeeds.set(key, value),
  removeItem: (key: string) => readerSeeds.delete(key),
  clear: () => readerSeeds.clear(),
};

vi.mock("@tauri-apps/api/event", () => eventMocks);
vi.mock("@tauri-apps/api/window", () => windowMocks);
vi.mock("@tauri-apps/api/webviewWindow", () => webviewMocks);

describe("reader window infrastructure", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    readerSeedStorage.clear();
    window.sessionStorage.clear();
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: readerSeedStorage,
    });
    window.history.replaceState({}, "", "/");
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
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
      seed,
    );
    expect(webviewMocks.WebviewWindow).not.toHaveBeenCalled();
  });

  it("rejects when native reader-window creation fails", async () => {
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      {};
    webviewMocks.getAllWebviewWindows.mockResolvedValue([]);
    webviewMocks.WebviewWindow.mockImplementation(function () {
      return {
        once: (
          event: string,
          handler: (payload: { payload: string }) => void,
        ) => {
          if (event === "tauri://error")
            queueMicrotask(() => handler({ payload: "denied" }));
        },
      };
    });
    const { openReaderWindow } = await import("./readerWindow");

    await expect(
      openReaderWindow({
        target: { accountId: "account-1", threadId: "thread-1" },
      }),
    ).rejects.toThrow("Could not open reader window: denied");
    expect(readerSeeds.size).toBe(0);
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
