import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

const settings = {
  provider: "openai" as const,
  baseUrl: "https://ai.example.test/v1",
  model: "fictional-model",
  apiKey: "test-key",
  executable: "",
  modelPath: "",
};

describe("native AI immutable message identity bridge", () => {
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

  it("sends exact immutable targets with every requested AI message", async () => {
    invoke.mockResolvedValue("ok");
    const { api } = await import("./api");
    const targets = [
      { messageId: "message-π-雪", expectedUidValidity: 77 },
      { messageId: "message-2", expectedUidValidity: 78 },
    ];

    await api.summarize(settings, targets);
    await api.draft(settings, targets, "Write a concise reply");

    expect(invoke.mock.calls).toEqual([
      [
        "ai_summarize",
        {
          input: {
            provider: "openai",
            baseUrl: "https://ai.example.test/v1",
            model: "fictional-model",
            apiKey: "test-key",
            executable: null,
            modelPath: null,
            messageIds: ["message-π-雪", "message-2"],
            messageTargets: targets,
            instruction: null,
          },
        },
      ],
      [
        "ai_draft",
        {
          input: {
            provider: "openai",
            baseUrl: "https://ai.example.test/v1",
            model: "fictional-model",
            apiKey: "test-key",
            executable: null,
            modelPath: null,
            messageIds: ["message-π-雪", "message-2"],
            messageTargets: targets,
            instruction: "Write a concise reply",
          },
        },
      ],
    ]);
  });

  it("preserves a legacy null epoch for native fail-closed validation", async () => {
    invoke.mockRejectedValue(new Error("message identity changed"));
    const { api } = await import("./api");

    await expect(
      api.summarize(settings, [
        { messageId: "legacy-message", expectedUidValidity: null },
      ]),
    ).rejects.toThrow("message identity changed");
    expect(invoke).toHaveBeenCalledWith("ai_summarize", {
      input: expect.objectContaining({
        messageIds: ["legacy-message"],
        messageTargets: [
          { messageId: "legacy-message", expectedUidValidity: null },
        ],
      }),
    });
  });
});
