import { convertFileSrc } from "@tauri-apps/api/core";
import {
  BatchTranslator,
  TranslatorBacking,
} from "@browsermt/bergamot-translator/translator.js";
import { api } from "./api";

type ModelDescriptor = { from: string; to: string };
type BergamotWorker = {
  hasTranslationModel: (pair: ModelDescriptor) => Promise<boolean>;
  loadTranslationModel: (
    pair: ModelDescriptor,
    buffers: TranslationBuffers,
  ) => Promise<void>;
  translate: (input: unknown) => Promise<unknown>;
};
type TranslationBuffers = {
  model: ArrayBuffer;
  shortlist: ArrayBuffer;
  vocabs: ArrayBuffer[];
  config: Record<string, string>;
};

export class DakiaTranslationBacking extends TranslatorBacking {
  async loadModelRegistery(): Promise<ModelDescriptor[]> {
    const models = await api.translationModels();
    return models.map(({ source: from }) => ({ from, to: "en" }));
  }

  async loadTranslationModel({
    from,
    to,
  }: ModelDescriptor): Promise<TranslationBuffers> {
    if (to !== "en") throw new Error("Dakia translates only to English.");
    const files = await api.translationModelFiles(from);
    const [model, shortlist, ...vocabs] = await Promise.all([
      readModelFile(files.modelPath),
      readModelFile(files.shortlistPath),
      ...files.vocabPaths.map(readModelFile),
    ]);
    return { model, shortlist, vocabs, config: files.config };
  }

  async loadWorker(): Promise<{
    worker: Worker;
    exports: BergamotWorker;
  }> {
    const worker = new Worker("/bergamot/translator-worker.js");
    let serial = 0;
    const pending = new Map<
      number,
      { resolve: (value: unknown) => void; reject: (error: Error) => void }
    >();
    const call = (name: string, ...args: unknown[]) =>
      new Promise<unknown>((resolve, reject) => {
        const id = ++serial;
        pending.set(id, { resolve, reject });
        worker.postMessage({ id, name, args });
      });

    worker.addEventListener(
      "message",
      ({
        data,
      }: MessageEvent<{
        id: number;
        result?: unknown;
        error?: { message?: string; stack?: string };
      }>) => {
        const waiting = pending.get(data.id);
        if (!waiting) return;
        pending.delete(data.id);
        if (data.error) {
          const error = new Error(data.error.message ?? "Translation failed");
          error.stack = data.error.stack;
          waiting.reject(error);
        } else {
          waiting.resolve(data.result);
        }
      },
    );
    worker.addEventListener("error", (event) => {
      const error = new Error(event.message || "Translation worker failed");
      for (const waiting of pending.values()) waiting.reject(error);
      pending.clear();
      this.onerror(error);
    });
    worker.addEventListener("messageerror", () => {
      const error = new Error(
        "Translation worker returned an unreadable response",
      );
      for (const waiting of pending.values()) waiting.reject(error);
      pending.clear();
      this.onerror(error);
    });

    await call("initialize", { cacheSize: 2048, useNativeIntGemm: false });
    const exports = new Proxy(
      {},
      {
        get:
          (_target, name) =>
          (...args: unknown[]) =>
            call(String(name), ...args),
      },
    ) as BergamotWorker;
    return { worker, exports };
  }
}

async function readModelFile(path: string) {
  if (!path) throw new Error("Offline translation requires the Dakia app.");
  const response = await fetch(convertFileSrc(path));
  if (!response.ok)
    throw new Error("Could not read the installed language pack.");
  return response.arrayBuffer();
}

let translator: BatchTranslator | undefined;
let translatorSource: string | undefined;
export const OFFLINE_TRANSLATION_TIMEOUT_MS = 120_000;

function prepareHtmlForTranslation(html: string) {
  const document = new DOMParser().parseFromString(html, "text/html");
  return {
    html: document.documentElement.outerHTML,
    hasVisibleText: Boolean(document.body.textContent?.trim()),
  };
}

export function normalizeHtmlForTranslation(html: string) {
  return prepareHtmlForTranslation(html).html;
}

async function withTranslationTimeout<T>(operation: Promise<T>) {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_resolve, reject) => {
        timeout = setTimeout(
          () => reject(new Error("Offline translation worker timed out.")),
          OFFLINE_TRANSLATION_TIMEOUT_MS,
        );
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}

async function getTranslator(source: string) {
  if (translator && translatorSource !== source) {
    await translator.delete();
    translator = undefined;
  }
  translator ??= new BatchTranslator(
    {
      workers: 1,
      batchSize: 1,
      cacheSize: 2048,
      pivotLanguage: null,
    },
    new DakiaTranslationBacking({
      cacheSize: 2048,
      pivotLanguage: null,
    }),
  );
  translatorSource = source;
  return translator;
}

export async function resetOfflineTranslator() {
  const activeTranslator = translator;
  translator = undefined;
  translatorSource = undefined;
  if (activeTranslator) await activeTranslator.delete();
}

export async function detectTranslationLanguage(text: string) {
  const normalized = text.replace(/\s+/g, " ").trim().slice(0, 10_000);
  if (normalized.length < 20)
    return {
      language: "und",
      languageName: "Unknown language",
      reliable: false,
    };
  return api.detectTranslationLanguage(normalized);
}

export async function translateOffline(
  source: string,
  text: string,
  html = false,
) {
  try {
    const preparedHtml = html ? prepareHtmlForTranslation(text) : undefined;
    if (html && !preparedHtml?.hasVisibleText)
      return preparedHtml?.html ?? text;
    if (!html && !text.trim()) return text;

    const response = await withTranslationTimeout(
      (await getTranslator(source)).translate({
        from: source,
        to: "en",
        text: preparedHtml?.html ?? text,
        html,
      }),
    );
    if (
      !response ||
      typeof response !== "object" ||
      !("target" in response) ||
      !response.target ||
      typeof response.target.text !== "string"
    ) {
      throw new Error(
        "Offline translation worker returned an invalid response.",
      );
    }
    return response.target.text;
  } catch (error) {
    console.error("Offline translation worker failed", error);
    try {
      await resetOfflineTranslator();
    } catch (resetError) {
      console.error(
        "Could not reset the offline translation worker",
        resetError,
      );
    }
    throw new Error("Offline translation could not be completed.");
  }
}
