import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const apiMocks = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: apiMocks.invoke,
  Channel: class {},
}));

const target = {
  accountId: "account-1",
  localMessageId: "message-1",
  threadId: "thread-1",
  mailbox: "INBOX",
};

describe("native thread-context API bridge", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    apiMocks.invoke.mockResolvedValue(undefined);
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("uses immutable conversation targets and revision UUIDs for every native mutation", async () => {
    const { api } = await import("./api");
    await api.threadContext(target);
    await api.updateThreadContext({
      target,
      kind: "agentMemory",
      content: "Claim number is 42",
      expectedRevision: 7,
    });
    await api.appendThreadContext({
      target,
      kind: "notes",
      text: "Called the insurer",
      expectedRevision: 3,
    });
    await api.clearThreadContext({
      target,
      kind: "notes",
      expectedRevision: 4,
    });
    await api.threadContextHistory(target, "notes");
    await api.restoreThreadContext(target, "notes", "revision-1", 8);

    expect(apiMocks.invoke.mock.calls).toEqual([
      ["thread_context", { target }],
      [
        "update_thread_context",
        {
          input: {
            target,
            kind: "agentMemory",
            content: "Claim number is 42",
            expectedRevision: 7,
          },
        },
      ],
      [
        "append_thread_context",
        {
          input: {
            target,
            kind: "notes",
            text: "Called the insurer",
            expectedRevision: 3,
          },
        },
      ],
      [
        "clear_thread_context",
        {
          input: { target, kind: "notes", expectedRevision: 4 },
        },
      ],
      ["thread_context_history", { target, kind: "notes" }],
      [
        "restore_thread_context",
        {
          input: {
            target,
            kind: "notes",
            revision: "revision-1",
            expectedRevision: 8,
          },
        },
      ],
    ]);
  });
});
