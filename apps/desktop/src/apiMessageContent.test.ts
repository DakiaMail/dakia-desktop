import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const apiMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: apiMocks.invoke,
  Channel: class {},
}));

describe("native message-content API bridge", () => {
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

  it("preserves the native content category while hiding its diagnostic detail", async () => {
    apiMocks.invoke.mockRejectedValue({
      kind: "undecodable",
      detail: "invalid bytes at offset 42",
    });
    const { api, MessageContentError } = await import("./api");

    await expect(api.content("message-1")).rejects.toMatchObject({
      name: "MessageContentError",
      kind: "undecodable",
      retryable: false,
      message: "Message content could not be loaded",
    });
    expect(apiMocks.invoke).toHaveBeenCalledWith("message_content", {
      messageId: "message-1",
    });
    await api.content("message-1").catch((error: unknown) => {
      expect(error).toBeInstanceOf(MessageContentError);
      expect(String(error)).not.toContain("offset 42");
    });
  });

  it("treats an untyped provider failure as transient and retryable", async () => {
    apiMocks.invoke.mockRejectedValue("IMAP connection reset by peer");
    const { api } = await import("./api");

    await expect(api.content("message-1")).rejects.toMatchObject({
      kind: "transient",
      retryable: true,
      message: "Message content could not be loaded",
    });
  });
});
