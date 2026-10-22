import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const windowOpen = vi.fn();
const webviewWindowCtor = vi.fn();
const coreMocks = vi.hoisted(() => ({ invoke: vi.fn() }));
const eventMocks = vi.hoisted(() => ({ emitTo: vi.fn(), listen: vi.fn() }));
const feedbackMocks = vi.hoisted(() => ({ showNativeMessage: vi.fn() }));

vi.mock("@tauri-apps/api/event", () => eventMocks);

vi.mock("@tauri-apps/api/core", () => coreMocks);

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: vi.fn(),
}));

vi.mock("@tauri-apps/api/webviewWindow", () => ({
  WebviewWindow: webviewWindowCtor,
}));

vi.mock("./nativeFeedback", () => feedbackMocks);

function installIndexedDb({
  holdFirstPut = false,
  abortFirstPut = false,
} = {}) {
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
              const onSuccess = request.onsuccess as
                ((event: Event) => void) | null;
              onSuccess?.(new Event("success"));
              transaction.oncomplete?.();
            });
            return request as IDBRequest;
          },
          put: (value, key) => {
            events.puts += 1;
            const complete = () => {
              records.set(key, value);
              transaction.oncomplete?.();
            };
            if (abortFirstPut && events.puts === 1) {
              queueMicrotask(() => {
                transaction.error = new DOMException(
                  "Transaction aborted",
                  "AbortError",
                );
                transaction.onabort?.();
              });
            } else if (holdFirstPut && events.puts === 1) {
              void firstPut.then(complete);
            } else {
              queueMicrotask(complete);
            }
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
  const indexedDb = {
    open: vi.fn(() => {
      const request: Partial<IDBOpenDBRequest> = {
        result: database as unknown as IDBDatabase,
      };
      queueMicrotask(() => {
        const onSuccess = request.onsuccess as ((event: Event) => void) | null;
        onSuccess?.(new Event("success"));
      });
      return request as IDBOpenDBRequest;
    }),
  };
  Object.defineProperty(globalThis, "indexedDB", {
    configurable: true,
    value: indexedDb,
  });
  return {
    events,
    releaseFirstPut: () => releaseFirstPut?.(),
  };
}

describe("composeWindow seed handoff", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    localStorage.clear();
    window.history.replaceState({}, "", "/");
    windowOpen.mockReset();
    webviewWindowCtor.mockReset();
    Object.defineProperty(window, "open", {
      configurable: true,
      writable: true,
      value: windowOpen,
    });
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("stores the compose seed behind a short token when opening a popup", async () => {
    const { openComposeWindow } = await import("./composeWindow");

    openComposeWindow({
      accountId: "account-1",
      to: "mara@example.com",
      contextMessageIds: Array.from(
        { length: 400 },
        (_, index) => `m-${index}`,
      ),
    });

    expect(windowOpen).toHaveBeenCalledOnce();
    const [url] = windowOpen.mock.calls[0];
    const params = new URL(String(url), "http://localhost").searchParams;
    expect(params.get("view")).toBe("compose");
    expect(params.get("seed")).toBeNull();
    const token = params.get("seedKey");
    expect(token).toBeTruthy();
    expect(localStorage.getItem(`dakia.compose-seed.${token}`)).toContain(
      '"accountId":"account-1"',
    );
    expect(
      JSON.parse(localStorage.getItem(`dakia.compose-seed.${token}`) ?? "{}"),
    ).toEqual(expect.objectContaining({ sendAttemptId: expect.any(String) }));
  });

  it("uses the Rust-owned compose-window command in Tauri", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    let ready:
      | ((event: { payload: { label: string; seedToken: string } }) => void)
      | undefined;
    eventMocks.listen.mockImplementation(async (event, handler) => {
      if (event === "compose-ready") ready = handler;
      return () => undefined;
    });
    coreMocks.invoke.mockImplementation(async (_command, args) => {
      const label = "compose-11111111-1111-4111-8111-111111111111";
      ready?.({ payload: { label, seedToken: args.seedDbKey } });
      return label;
    });
    const { openComposeWindow } = await import("./composeWindow");

    openComposeWindow({ to: "mara@example.com" });

    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(coreMocks.invoke).toHaveBeenCalledWith("open_compose_window", {
      seedKey: null,
      seedDbKey: expect.any(String),
    });
    const seed = { to: "mara@example.com" };
    expect(eventMocks.emitTo).toHaveBeenCalledWith(
      "compose-11111111-1111-4111-8111-111111111111",
      "compose-target",
      expect.objectContaining({ seed: expect.objectContaining(seed) }),
    );
    expect(
      Array.from({ length: localStorage.length }, (_, index) =>
        localStorage.key(index),
      ).some((key) => key?.startsWith("dakia.compose-seed.")),
    ).toBe(false);
    expect(webviewWindowCtor).not.toHaveBeenCalled();
  });

  it("opens a native Composer without relying on opener IndexedDB", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    let ready:
      | ((event: { payload: { label: string; seedToken: string } }) => void)
      | undefined;
    eventMocks.listen.mockImplementation(async (event, handler) => {
      if (event === "compose-ready") ready = handler;
      return () => undefined;
    });
    coreMocks.invoke.mockImplementation(async (_command, args) => {
      const label = "compose-22222222-2222-4222-8222-222222222222";
      ready?.({ payload: { label, seedToken: args.seedDbKey } });
      return label;
    });
    const { openComposeWindow } = await import("./composeWindow");
    openComposeWindow({ to: "mara@example.com" });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(coreMocks.invoke).toHaveBeenCalledOnce();
  });

  it("delivers an exact native target only to the matching Compose label and token", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const token = "11111111-1111-4111-8111-111111111111";
    const label = "compose-22222222-2222-4222-8222-222222222222";
    window.history.replaceState({}, "", `/?view=compose&seedDbKey=${token}`);
    vi.mocked(
      (await import("@tauri-apps/api/window")).getCurrentWindow,
    ).mockReturnValue({
      label,
    } as never);
    let listener: ((event: { payload: unknown }) => void) | undefined;
    eventMocks.listen.mockImplementation(async (_event, handler) => {
      listener = handler;
      return () => undefined;
    });
    const { onComposeTarget } = await import("./composeWindow");
    const received = vi.fn();
    await onComposeTarget(received);

    listener?.({
      payload: {
        label,
        seedToken: "wrong",
        seed: { to: "wrong@example.test" },
      },
    });
    listener?.({
      payload: {
        label: "compose-33333333-3333-4333-8333-333333333333",
        seedToken: token,
        seed: { to: "wrong@example.test" },
      },
    });
    const attachment = {
      filename: "résumé.pdf",
      mime_type: "application/pdf",
      content_base64: "AA==",
      size_bytes: 1,
    };
    listener?.({
      payload: {
        label,
        seedToken: token,
        seed: {
          accountId: "account-1",
          to: "right@example.test",
          subject: "Re: exact",
          attachments: [attachment],
        },
      },
    });

    expect(received).toHaveBeenCalledOnce();
    expect(received).toHaveBeenCalledWith(
      expect.objectContaining({
        accountId: "account-1",
        to: "right@example.test",
        subject: "Re: exact",
        attachments: [attachment],
      }),
    );
  });

  it("shows feedback when native Composer creation fails", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    coreMocks.invoke.mockRejectedValueOnce(new Error("window creation denied"));
    eventMocks.listen.mockResolvedValue(() => undefined);
    const { openComposeWindow } = await import("./composeWindow");

    openComposeWindow({ to: "mara@example.com" });
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(feedbackMocks.showNativeMessage).toHaveBeenCalledWith(
      "Could not open a new message",
      "Try again.",
      "error",
    );
  });

  it("reads a stored compose seed token with a stable legacy attempt id", async () => {
    const token = "8f906af2-516d-4d4a-9c04-a42efbe8a698";
    localStorage.setItem(
      `dakia.compose-seed.${token}`,
      JSON.stringify({
        subject: "Re: Weekly notes",
        cc: "team@example.com",
        contextMessageIds: ["message-1", "message-2"],
      }),
    );
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);

    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({
      subject: "Re: Weekly notes",
      cc: "team@example.com",
      contextMessageIds: ["message-1", "message-2"],
      sendAttemptId: token,
    });
    expect(localStorage.getItem(`dakia.compose-seed.${token}`)).toContain(
      '"subject":"Re: Weekly notes"',
    );
  });

  it("does not restore a native session seed for another child label or token", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    const token = "8f906af2-516d-4d4a-9c04-a42efbe8a698";
    const label = "compose-11111111-1111-4111-8111-111111111111";
    window.history.replaceState({}, "", `/?view=compose&seedDbKey=${token}`);
    vi.mocked(
      (await import("@tauri-apps/api/window")).getCurrentWindow,
    ).mockReturnValue({ label } as never);
    sessionStorage.setItem(
      "dakia.compose-active-seed",
      JSON.stringify({
        label: "compose-22222222-2222-4222-8222-222222222222",
        seedDbKey: token,
        seed: { to: "stale@example.test" },
      }),
    );

    const { hasNativeComposeSeedHandoff, readComposeSeed } =
      await import("./composeWindow");
    expect(hasNativeComposeSeedHandoff()).toBe(true);
    expect(readComposeSeed()).toEqual({ sendAttemptId: token });

    sessionStorage.setItem(
      "dakia.compose-active-seed",
      JSON.stringify({
        label,
        seedDbKey: "f7b1c6d9-7403-47d4-8f55-9c010be269f9",
        seed: { to: "stale@example.test" },
      }),
    );
    expect(hasNativeComposeSeedHandoff()).toBe(true);
    sessionStorage.clear();
  });

  it("preserves the assigned attempt id from opening through seed recovery", async () => {
    const { openComposeWindow, readComposeSeed } =
      await import("./composeWindow");

    openComposeWindow({ to: "mara@example.com" });
    const url = new URL(
      String(windowOpen.mock.calls[0][0]),
      "http://localhost",
    );
    const token = url.searchParams.get("seedKey");
    const stored = JSON.parse(
      localStorage.getItem(`dakia.compose-seed.${token}`) ?? "{}",
    );
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);

    expect(readComposeSeed()).toEqual({
      to: "mara@example.com",
      sendAttemptId: stored.sendAttemptId,
    });
  });

  it("falls back to the inline seed for existing compose links", async () => {
    const seed = encodeURIComponent(
      JSON.stringify({ to: "legacy@example.com" }),
    );
    window.history.replaceState({}, "", `/?view=compose&seed=${seed}`);

    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({ to: "legacy@example.com" });
  });

  it("reports when an inline legacy compose link cannot persist an active draft", async () => {
    const seed = encodeURIComponent(
      JSON.stringify({ to: "legacy@example.com" }),
    );
    window.history.replaceState({}, "", `/?view=compose&seed=${seed}`);
    const { persistComposeSeed } = await import("./composeWindow");

    await expect(
      persistComposeSeed({ subject: "Needs a durable token" }),
    ).rejects.toThrow("Compose draft storage is unavailable.");
  });

  it("retains a stable attempt when a reload has a seed token but no draft record", async () => {
    const token = "55e01e1a-95b8-4552-a15a-c0ec25bb6a8d";
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({ sendAttemptId: token });
  });

  it("retains a stable attempt when a database-backed seed record is unavailable", async () => {
    const token = "729d050e-3d72-457c-a564-4dd4f0f05c90";
    window.history.replaceState({}, "", `/?view=compose&seedDbKey=${token}`);
    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({ sendAttemptId: token });
  });

  it("updates the active token-scoped draft without consuming it", async () => {
    const token = "47f7fc2f-5058-4b5c-b074-1ef2d0765b02";
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    const { persistComposeSeed, readComposeSeed } =
      await import("./composeWindow");

    persistComposeSeed({
      sendAttemptId: token,
      subject: "Draft survives reload",
      bodyHtml: "<p>Draft body</p>",
      sendState: "sending",
    });

    expect(readComposeSeed()).toEqual({
      sendAttemptId: token,
      subject: "Draft survives reload",
      bodyHtml: "<p>Draft body</p>",
      sendState: "sending",
    });
  });

  it("persists a 25 MiB attachment's base64 payload in IndexedDB when localStorage quota is exhausted", async () => {
    const token = "d1ce2d3b-cf08-4f27-80b5-4584308b3a3b";
    localStorage.setItem(
      `dakia.compose-seed.${token}`,
      JSON.stringify({ sendAttemptId: token, subject: "Initial draft" }),
    );
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    installIndexedDb();
    const quotaExceeded = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw new DOMException("Quota exceeded", "QuotaExceededError");
      });
    const { persistComposeSeed, readDatabaseComposeSeed } =
      await import("./composeWindow");
    const rawAttachmentBytes = 25 * 1024 * 1024;
    const attachment = {
      filename: "permitted-25mb.pdf",
      mime_type: "application/pdf",
      // A browser FileReader uses four base64 bytes for each three raw bytes.
      // This is the actual persisted size for Composer's permitted per-file
      // boundary, rather than a smaller synthetic encoded payload.
      content_base64: "A".repeat(Math.ceil(rawAttachmentBytes / 3) * 4),
      size_bytes: rawAttachmentBytes,
    };

    await persistComposeSeed({
      sendAttemptId: token,
      subject: "Large attachment draft",
      attachments: [attachment],
      sendState: "delivery_uncertain",
    });

    expect(quotaExceeded).not.toHaveBeenCalled();
    await expect(readDatabaseComposeSeed()).resolves.toEqual({
      sendAttemptId: token,
      subject: "Large attachment draft",
      attachments: [attachment],
      sendState: "delivery_uncertain",
    });
  });

  it("rejects an IndexedDB transaction abort instead of leaving draft persistence pending", async () => {
    const token = "32021535-80c1-4711-b739-4ccaf2a3af7d";
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    installIndexedDb({ abortFirstPut: true });
    const { persistComposeSeed } = await import("./composeWindow");

    await expect(
      persistComposeSeed({ sendAttemptId: token, subject: "Abort safely" }),
    ).rejects.toThrow("Transaction aborted");
  });

  it("waits for an older active-draft write before deleting it after confirmed send", async () => {
    const token = "04b788d7-a10d-4bf0-b84d-952a13b6aed6";
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    const indexed = installIndexedDb({ holdFirstPut: true });
    const { discardComposeSeed, persistComposeSeed, readDatabaseComposeSeed } =
      await import("./composeWindow");

    const write = persistComposeSeed({
      sendAttemptId: token,
      subject: "Do not resurrect",
    });
    const discard = discardComposeSeed();
    await Promise.resolve();
    expect(indexed.events.deletes).toBe(0);

    indexed.releaseFirstPut();
    await Promise.all([write, discard]);

    expect(indexed.events).toEqual({ puts: 1, deletes: 1 });
    await expect(readDatabaseComposeSeed()).resolves.toBeUndefined();
  });

  it("removes a delivered draft only when it is explicitly discarded", async () => {
    const token = "d55d0792-e893-4ebd-9f16-a4bf3e42c8c4";
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);
    const { discardComposeSeed, persistComposeSeed, readComposeSeed } =
      await import("./composeWindow");

    await persistComposeSeed({
      sendAttemptId: token,
      subject: "Delivered draft",
    });
    await discardComposeSeed();

    expect(readComposeSeed()).toEqual({ sendAttemptId: token });
  });

  it("assigns different attempts to separately opened compose windows", async () => {
    vi.spyOn(crypto, "randomUUID")
      .mockReturnValueOnce("11111111-1111-4111-8111-111111111111")
      .mockReturnValueOnce("22222222-2222-4222-8222-222222222222")
      .mockReturnValueOnce("33333333-3333-4333-8333-333333333333")
      .mockReturnValueOnce("44444444-4444-4444-8444-444444444444");
    const { openComposeWindow } = await import("./composeWindow");

    openComposeWindow({ to: "same@example.com" });
    openComposeWindow({ to: "same@example.com" });

    const firstUrl = new URL(
      String(windowOpen.mock.calls[0][0]),
      "http://localhost",
    );
    const secondUrl = new URL(
      String(windowOpen.mock.calls[1][0]),
      "http://localhost",
    );
    const first = JSON.parse(
      localStorage.getItem(
        `dakia.compose-seed.${firstUrl.searchParams.get("seedKey")}`,
      ) ?? "{}",
    );
    const second = JSON.parse(
      localStorage.getItem(
        `dakia.compose-seed.${secondUrl.searchParams.get("seedKey")}`,
      ) ?? "{}",
    );

    expect(first.sendAttemptId).toBe("11111111-1111-4111-8111-111111111111");
    expect(second.sendAttemptId).toBe("33333333-3333-4333-8333-333333333333");
  });

  it("accepts a faithful recovery seed and preserves an explicit plain-text-only body", async () => {
    const { composeSeedFromSendRecovery } = await import("./composeWindow");
    const accountId = "b9557475-508f-48b0-b492-d36b65fb2ecc";
    expect(
      composeSeedFromSendRecovery({
        accountId,
        attemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
        status: "not_submitted",
        message: {
          accountId,
          to: ["recover@example.test"],
          cc: [],
          bcc: [],
          subject: "Exact recovery",
          bodyText: "Plain source",
          bodyHtml: null,
          inReplyTo: null,
          references: null,
          attachments: [
            {
              filename: "message.txt",
              mimeType: "text/plain",
              contentBase64: "AQI=",
              sizeBytes: 2,
            },
          ],
        },
      }),
    ).toEqual(
      expect.objectContaining({
        accountId,
        body: "Plain source",
        bodyHtml: null,
        attachments: [
          expect.objectContaining({
            filename: "message.txt",
            mime_type: "text/plain",
            content_base64: "AQI=",
            size_bytes: 2,
          }),
        ],
      }),
    );
  });

  it("rejects a recovery seed with forged attachment size before it reaches Composer", async () => {
    const { composeSeedFromSendRecovery } = await import("./composeWindow");
    const accountId = "b9557475-508f-48b0-b492-d36b65fb2ecc";
    expect(() =>
      composeSeedFromSendRecovery({
        accountId,
        attemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
        status: "not_submitted",
        message: {
          accountId,
          to: [],
          cc: [],
          bcc: [],
          subject: "Rejected recovery",
          bodyText: "",
          bodyHtml: null,
          inReplyTo: null,
          references: null,
          attachments: [
            {
              filename: "message.txt",
              mimeType: "text/plain",
              contentBase64: "AQI=",
              sizeBytes: 3,
            },
          ],
        },
      }),
    ).toThrow("inconsistent attachment data");
  });
});
