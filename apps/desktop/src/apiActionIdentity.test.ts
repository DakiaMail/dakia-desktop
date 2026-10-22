import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

describe("native mailbox-action identity bridge", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("preserves the captured non-ASCII message identity in the native action", async () => {
    let resolveInvoke: () => void = () => undefined;
    invoke.mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveInvoke = resolve;
        }),
    );
    const { api } = await import("./api");

    const target = {
      accountId: "account-α",
      mailbox: "收件箱/Архив",
      uid: 42,
      action: "trash" as const,
      messageId: "message-π-雪",
      expectedUidValidity: 424242,
    };
    const request = api.action(
      target.accountId,
      target.mailbox,
      target.uid,
      target.action,
      target.messageId,
      target.expectedUidValidity,
    );
    target.messageId = "later-active-message";

    expect(invoke).toHaveBeenCalledWith("apply_mailbox_action", {
      accountId: "account-α",
      mailbox: "收件箱/Архив",
      uid: 42,
      action: "trash",
      messageId: "message-π-雪",
      expectedUidValidity: 424242,
    });
    resolveInvoke();
    await request;
  });

  it("sends an explicit null identity for legacy callers", async () => {
    invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.action("account-1", "INBOX", 7, "archive");

    expect(invoke).toHaveBeenCalledWith("apply_mailbox_action", {
      accountId: "account-1",
      mailbox: "INBOX",
      uid: 7,
      action: "archive",
      messageId: null,
      expectedUidValidity: null,
    });
  });

  it("forwards the same captured epoch through read and starred mutations", async () => {
    invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.setRead("message-π-雪", true, 424242);
    await api.setStarred("message-π-雪", false, 424242);

    expect(invoke.mock.calls).toEqual([
      [
        "set_message_read",
        {
          messageId: "message-π-雪",
          read: true,
          expectedUidValidity: 424242,
        },
      ],
      [
        "set_message_starred",
        {
          messageId: "message-π-雪",
          starred: false,
          expectedUidValidity: 424242,
        },
      ],
    ]);
  });

  it("binds unsubscribe to the captured mailbox epoch", async () => {
    invoke.mockResolvedValue({ kind: "completed", cleanupTarget: null });
    const { api } = await import("./api");

    await api.unsubscribe("message-π-雪", 424242);

    expect(invoke).toHaveBeenCalledWith("unsubscribe_message", {
      messageId: "message-π-雪",
      expectedUidValidity: 424242,
    });
  });

  it("binds category changes to the captured mailbox epoch", async () => {
    invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.setCategory("message-π-雪", "travel", 424242);

    expect(invoke).toHaveBeenCalledWith("set_message_category", {
      messageId: "message-π-雪",
      category: "travel",
      expectedUidValidity: 424242,
    });
  });
});
