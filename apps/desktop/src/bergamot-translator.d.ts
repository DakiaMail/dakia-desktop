declare module "@browsermt/bergamot-translator/translator.js" {
  export type TranslationRequest = {
    from: string;
    to: string;
    text: string;
    html?: boolean;
    priority?: number;
  };

  export class TranslatorBacking {
    constructor(options?: Record<string, unknown>);
    options: Record<string, unknown>;
    registry: Promise<Array<{ from: string; to: string }>>;
    onerror: (error: Error) => void;
    loadModelRegistery(): Promise<Array<{ from: string; to: string }>>;
    loadWorker(): Promise<{
      worker: Worker;
      exports: Record<string, Function>;
    }>;
  }

  export class BatchTranslator {
    constructor(options?: Record<string, unknown>, backing?: TranslatorBacking);
    translate(
      request: TranslationRequest,
    ): Promise<{ target: { text: string } }>;
    delete(): Promise<void>;
  }
}
