import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

describe("forward attachment API bridge", () => {
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

  it("preserves the captured mailbox epoch for forward attachment fetches", async () => {
    const { api } = await import("./api");

    await api.forwardAttachments("message-1", 90210);

    expect(invoke).toHaveBeenCalledWith("forward_attachments", {
      messageId: "message-1",
      expectedUidValidity: 90210,
    });
  });

  it("sends an explicit null epoch for legacy compose seeds", async () => {
    const { api } = await import("./api");

    await api.forwardAttachments("message-1");

    expect(invoke).toHaveBeenCalledWith("forward_attachments", {
      messageId: "message-1",
      expectedUidValidity: null,
    });
  });
});
