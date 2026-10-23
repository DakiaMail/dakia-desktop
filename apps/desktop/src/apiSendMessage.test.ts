import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

describe("native send-message API bridge", () => {
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

  it("decodes the validated native Sent-copy failure without exposing its diagnostic", async () => {
    invoke.mockRejectedValue(
      JSON.stringify({
        kind: "sent_copy_failed",
        message: "IMAP append failed for imap.private.example",
      }),
    );
    const { api, SendMessageError } = await import("./api");

    await expect(api.send({ subject: "Hello" })).rejects.toMatchObject({
      name: "SendMessageError",
      kind: "sent_copy_failed",
      message: "Message was sent but no Sent copy was saved.",
    });
    await api.send({ subject: "Hello" }).catch((error: unknown) => {
      expect(error).toBeInstanceOf(SendMessageError);
      expect(String(error)).not.toContain("imap.private.example");
    });
    expect(invoke).toHaveBeenCalledWith("send_message", {
      draft: { subject: "Hello" },
    });
  });

  it("treats malformed or unknown native failures as delivery-uncertain mail", async () => {
    invoke.mockRejectedValue('{"kind":"arbitrary","message":"secret"}');
    const { api } = await import("./api");

    await expect(api.send({ subject: "Hello" })).rejects.toMatchObject({
      kind: "delivery_uncertain",
      message: "Delivery status is uncertain.",
    });
  });
});
