import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const apiMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  channels: [] as Array<{ onmessage?: (message: unknown) => void }>,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: apiMocks.invoke,
  Channel: class {
    onmessage?: (message: unknown) => void;

    constructor() {
      apiMocks.channels.push(this);
    }
  },
}));

describe("native translation API bridge", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    apiMocks.channels.length = 0;
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("maps model, detection, cancellation, and removal calls to exact Tauri commands", async () => {
    apiMocks.invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.translationModels();
    await api.translationModelFiles("et");
    await api.detectTranslationLanguage("Tere maailm");
    await api.cancelTranslationModelInstall("et");
    await api.removeTranslationModel("et");

    expect(apiMocks.invoke.mock.calls).toEqual([
      ["translation_models"],
      ["translation_model_files", { source: "et" }],
      ["translation_detect_language", { text: "Tere maailm" }],
      ["translation_cancel_install", { source: "et" }],
      ["translation_remove_model", { source: "et" }],
    ]);
  });

  it("bridges native download progress and returns installed model files", async () => {
    const progress = {
      source: "et",
      downloadedBytes: 10,
      totalBytes: 20,
      fileIndex: 1,
      fileCount: 3,
    };
    const files = {
      source: "et",
      target: "en" as const,
      modelPath: "/model",
      shortlistPath: "/shortlist",
      vocabPaths: ["/vocab"],
      config: {},
    };
    apiMocks.invoke.mockImplementation(
      async (command: string, args?: Record<string, unknown>) => {
        if (command === "translation_install_model") {
          (
            args?.onProgress as { onmessage?: (message: unknown) => void }
          ).onmessage?.(progress);
          return files;
        }
        return undefined;
      },
    );
    const onProgress = vi.fn();
    const { api } = await import("./api");

    await expect(
      api.installTranslationModel("et", onProgress),
    ).resolves.toEqual(files);

    expect(apiMocks.invoke).toHaveBeenCalledWith("translation_install_model", {
      source: "et",
      onProgress: apiMocks.channels[0],
    });
    expect(onProgress).toHaveBeenCalledWith(progress);
  });
});
