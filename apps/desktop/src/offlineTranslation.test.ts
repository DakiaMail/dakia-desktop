import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  translationModels: vi.fn(),
  translationModelFiles: vi.fn(),
  detectTranslationLanguage: vi.fn(),
  translate: vi.fn(),
  delete: vi.fn(),
  convertFileSrc: vi.fn((path: string) => `asset://localhost/${path}`),
}));

vi.mock("./api", () => ({
  api: {
    translationModels: mocks.translationModels,
    translationModelFiles: mocks.translationModelFiles,
    detectTranslationLanguage: mocks.detectTranslationLanguage,
  },
}));

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: mocks.convertFileSrc,
}));

vi.mock("@browsermt/bergamot-translator/translator.js", () => ({
  TranslatorBacking: class {
    options: Record<string, unknown>;
    onerror = vi.fn();

    constructor(options: Record<string, unknown> = {}) {
      this.options = options;
    }
  },
  BatchTranslator: class {
    translate = mocks.translate;
    delete = mocks.delete;
  },
}));

import {
  DakiaTranslationBacking,
  detectTranslationLanguage,
  normalizeHtmlForTranslation,
  OFFLINE_TRANSLATION_TIMEOUT_MS,
  resetOfflineTranslator,
  translateOffline,
} from "./offlineTranslation";

describe("offline translation runtime", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.translationModels.mockResolvedValue([
      {
        source: "ar",
        sourceName: "Arabic",
        target: "en",
        downloadBytes: 37_000_000,
        installed: false,
      },
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 37_000_000,
        installed: true,
      },
      {
        source: "ja",
        sourceName: "Japanese",
        target: "en",
        downloadBytes: 70_000_000,
        installed: false,
      },
      {
        source: "zh",
        sourceName: "Chinese",
        target: "en",
        downloadBytes: 70_000_000,
        installed: false,
      },
    ]);
    mocks.translationModelFiles.mockResolvedValue({
      source: "et",
      target: "en",
      modelPath: "/models/model.bin",
      shortlistPath: "/models/shortlist.bin",
      vocabPaths: ["/models/source.spm", "/models/target.spm"],
      config: { "beam-size": "1" },
    });
    mocks.detectTranslationLanguage.mockResolvedValue({
      language: "et",
      languageName: "Estonian",
      reliable: true,
    });
    mocks.translate.mockResolvedValue({ target: { text: "Hello" } });
    mocks.delete.mockResolvedValue(undefined);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => ({
        ok: true,
        arrayBuffer: async () => new TextEncoder().encode(url).buffer,
      })),
    );
  });

  it("derives the worker registry from the native pinned manifest", async () => {
    const backing = new DakiaTranslationBacking({});

    await expect(backing.loadModelRegistery()).resolves.toEqual([
      { from: "ar", to: "en" },
      { from: "et", to: "en" },
      { from: "ja", to: "en" },
      { from: "zh", to: "en" },
    ]);
    expect(mocks.translationModels).toHaveBeenCalled();
  });

  it("loads every verified model artifact through Tauri's local asset protocol", async () => {
    const backing = new DakiaTranslationBacking({});

    const buffers = await backing.loadTranslationModel({
      from: "et",
      to: "en",
    });

    expect(mocks.translationModelFiles).toHaveBeenCalledWith("et");
    expect(mocks.convertFileSrc.mock.calls.map(([path]) => path)).toEqual([
      "/models/model.bin",
      "/models/shortlist.bin",
      "/models/source.spm",
      "/models/target.spm",
    ]);
    expect(fetch).toHaveBeenCalledTimes(4);
    expect(buffers.model.byteLength).toBeGreaterThan(0);
    expect(buffers.shortlist.byteLength).toBeGreaterThan(0);
    expect(buffers.vocabs).toHaveLength(2);
    expect(buffers.config).toEqual({ "beam-size": "1" });
  });

  it("rejects non-English targets and unreadable local artifacts", async () => {
    const backing = new DakiaTranslationBacking({});
    await expect(
      backing.loadTranslationModel({ from: "et", to: "de" }),
    ).rejects.toThrow("only to English");

    vi.mocked(fetch).mockResolvedValueOnce({
      ok: false,
    } as Response);
    await expect(
      backing.loadTranslationModel({ from: "et", to: "en" }),
    ).rejects.toThrow("Could not read");
  });

  it("uses a dedicated worker RPC channel and initializes local WASM settings", async () => {
    class WorkerStub {
      static instance: WorkerStub;
      listeners = new Map<string, Array<(event: MessageEvent) => void>>();
      messages: Array<{ id: number; name: string; args: unknown[] }> = [];
      url: string;

      constructor(url: string) {
        this.url = url;
        WorkerStub.instance = this;
      }

      addEventListener(name: string, callback: (event: MessageEvent) => void) {
        const listeners = this.listeners.get(name) ?? [];
        listeners.push(callback);
        this.listeners.set(name, listeners);
      }

      postMessage(message: { id: number; name: string; args: unknown[] }) {
        this.messages.push(message);
        queueMicrotask(() => {
          for (const listener of this.listeners.get("message") ?? []) {
            listener({
              data: { id: message.id, result: true },
            } as MessageEvent);
          }
        });
      }
    }
    vi.stubGlobal("Worker", WorkerStub);
    const backing = new DakiaTranslationBacking({});

    const loaded = await backing.loadWorker();
    await loaded.exports.hasTranslationModel({ from: "et", to: "en" });

    expect(WorkerStub.instance.url).toBe("/bergamot/translator-worker.js");
    expect(WorkerStub.instance.messages).toEqual([
      {
        id: 1,
        name: "initialize",
        args: [{ cacheSize: 2048, useNativeIntGemm: false }],
      },
      {
        id: 2,
        name: "hasTranslationModel",
        args: [{ from: "et", to: "en" }],
      },
    ]);
  });

  it("rejects worker RPC errors and unreadable worker messages", async () => {
    class WorkerStub {
      listeners = new Map<string, Array<(event: MessageEvent) => void>>();
      calls = 0;

      addEventListener(name: string, callback: (event: MessageEvent) => void) {
        const listeners = this.listeners.get(name) ?? [];
        listeners.push(callback);
        this.listeners.set(name, listeners);
      }

      postMessage(message: { id: number }) {
        this.calls += 1;
        if (this.calls === 3) return;
        queueMicrotask(() => {
          for (const listener of this.listeners.get("message") ?? []) {
            listener({
              data:
                this.calls === 1
                  ? { id: message.id, result: true }
                  : {
                      id: message.id,
                      error: { message: "Worker model load failed" },
                    },
            } as MessageEvent);
          }
        });
      }
    }
    vi.stubGlobal("Worker", WorkerStub);
    const backing = new DakiaTranslationBacking({});
    const loaded = await backing.loadWorker();
    await expect(
      loaded.exports.hasTranslationModel({ from: "et", to: "en" }),
    ).rejects.toThrow("Worker model load failed");

    const pending = loaded.exports.hasTranslationModel({
      from: "et",
      to: "en",
    });
    for (const listener of (
      loaded.worker as unknown as WorkerStub
    ).listeners.get("messageerror") ?? []) {
      listener({} as MessageEvent);
    }
    await expect(pending).rejects.toThrow("unreadable response");
  });

  it("delegates language detection to the native offline detector", async () => {
    await expect(
      detectTranslationLanguage(
        "See on piisavalt pikk eestikeelne tekst tuvastamiseks.",
      ),
    ).resolves.toEqual({
      language: "et",
      languageName: "Estonian",
      reliable: true,
    });
    expect(mocks.detectTranslationLanguage).toHaveBeenCalledWith(
      "See on piisavalt pikk eestikeelne tekst tuvastamiseks.",
    );

    await expect(detectTranslationLanguage("short")).resolves.toEqual({
      language: "und",
      languageName: "Unknown language",
      reliable: false,
    });
    expect(mocks.detectTranslationLanguage).toHaveBeenCalledTimes(1);
  });

  it("translates with the local batch engine, unloading before switching source languages", async () => {
    await expect(translateOffline("et", "<p>Tere</p>", true)).resolves.toBe(
      "Hello",
    );
    expect(mocks.translate).toHaveBeenCalledWith({
      from: "et",
      to: "en",
      text: "<html><head></head><body><p>Tere</p></body></html>",
      html: true,
    });

    await expect(translateOffline("ar", "مرحبا", false)).resolves.toBe("Hello");
    expect(mocks.delete).toHaveBeenCalledOnce();

    await resetOfflineTranslator();
    expect(mocks.delete).toHaveBeenCalledTimes(2);
  });

  it("normalizes concatenated email documents before Bergamot sees their HTML", () => {
    const malformedEmail = [
      "<!doctype html><html><head><style>p{color:red}</style></head>",
      "<body><p>Tere esimene dokument.</p></body></html>",
      '<html xmlns:o="urn:schemas-microsoft-com:office:office"><head>',
      "<style>.outlook{font-family:Arial}</style></head>",
      '<body><table><tr><td class="outlook"><a href="https://example.test/path?x=1&amp;y=2">',
      "Tere teine dokument.</a></td></tr></table></body></html>",
    ].join("");

    const normalized = normalizeHtmlForTranslation(malformedEmail);
    const parsed = new DOMParser().parseFromString(normalized, "text/html");

    expect(normalized.match(/<html(?:\s|>)/g)).toHaveLength(1);
    expect(normalized.match(/<body(?:\s|>)/g)).toHaveLength(1);
    expect(parsed.body.textContent).toContain("Tere esimene dokument.");
    expect(parsed.body.textContent).toContain("Tere teine dokument.");
    expect(parsed.querySelector("a")?.href).toBe(
      "https://example.test/path?x=1&y=2",
    );
    expect(parsed.querySelector(".outlook")).not.toBeNull();
  });

  it.each([
    [
      "unclosed table markup",
      "<html><body><table><tr><td>Tere katkine tabel",
      "Tere katkine tabel",
    ],
    [
      "Office conditional comments",
      "<html><body><!--[if mso]><table><tr><td><![endif]--><p>Tere Office</p><!--[if mso]></td></tr></table><![endif]--></body></html>",
      "Tere Office",
    ],
    [
      "Unicode and entities",
      "<body><p>Tere &amp; head aega — kodu 🏠 e\u0301</p></body>",
      "Tere & head aega — kodu 🏠 é",
    ],
    [
      "forwarded document fragments",
      "<!doctype html><html><body><p>Esimene</p></body></html><!doctype html><html><body><div>Teine</div></body></html>",
      "EsimeneTeine",
    ],
  ])(
    "normalizes %s into one parseable document",
    (_name, html, visibleText) => {
      const normalized = normalizeHtmlForTranslation(html);
      const parsed = new DOMParser().parseFromString(normalized, "text/html");

      expect(normalized.match(/<html(?:\s|>)/g)).toHaveLength(1);
      expect(normalized.match(/<body(?:\s|>)/g)).toHaveLength(1);
      expect(parsed.body.textContent?.replace(/\s+/g, "")).toContain(
        visibleText.replace(/\s+/g, ""),
      );
    },
  );

  it("retires an aborted worker and exposes only a stable runtime error", async () => {
    const runtimeFailure = new Error(
      "Aborted(). Build with -s ASSERTIONS=1 for more info.",
    );
    const consoleError = vi
      .spyOn(console, "error")
      .mockImplementation(() => undefined);
    mocks.translate.mockRejectedValueOnce(runtimeFailure);

    await expect(translateOffline("et", "Tere", false)).rejects.toThrow(
      "Offline translation could not be completed.",
    );
    expect(mocks.delete).toHaveBeenCalledOnce();
    expect(consoleError).toHaveBeenCalledWith(
      "Offline translation worker failed",
      runtimeFailure,
    );

    consoleError.mockRestore();
  });

  it("rejects malformed worker responses and starts cleanly next time", async () => {
    const consoleError = vi
      .spyOn(console, "error")
      .mockImplementation(() => undefined);
    mocks.translate
      .mockResolvedValueOnce({ unexpected: true })
      .mockResolvedValueOnce({ target: { text: "Recovered" } });

    await expect(translateOffline("et", "Tere", false)).rejects.toThrow(
      "Offline translation could not be completed.",
    );
    expect(mocks.delete).toHaveBeenCalledOnce();
    await expect(translateOffline("et", "Tere jälle", false)).resolves.toBe(
      "Recovered",
    );

    consoleError.mockRestore();
  });

  it("times out a hung worker, retires it, and hides its diagnostic", async () => {
    vi.useFakeTimers();
    const consoleError = vi
      .spyOn(console, "error")
      .mockImplementation(() => undefined);
    mocks.translate.mockReturnValueOnce(new Promise(() => undefined));

    const operation = translateOffline("et", "Tere", false);
    const rejection = expect(operation).rejects.toThrow(
      "Offline translation could not be completed.",
    );
    await vi.advanceTimersByTimeAsync(OFFLINE_TRANSLATION_TIMEOUT_MS);

    await rejection;
    expect(mocks.delete).toHaveBeenCalledOnce();

    consoleError.mockRestore();
    vi.useRealTimers();
  });

  it("skips the worker for empty text and markup-only HTML", async () => {
    await expect(translateOffline("et", " \n\t", false)).resolves.toBe(" \n\t");
    await expect(
      translateOffline(
        "et",
        '<html><head><style>p{color:red}</style></head><body><img alt="Logo"></body></html>',
        true,
      ),
    ).resolves.toContain('<img alt="Logo">');
    expect(mocks.translate).not.toHaveBeenCalled();
  });
});
