import { BatchTranslator } from "@browsermt/bergamot-translator/translator.js";
import {
  DakiaTranslationBacking,
  normalizeHtmlForTranslation,
} from "../offlineTranslation";

class FixtureTranslationBacking extends DakiaTranslationBacking {
  async loadModelRegistery() {
    return ["ar", "et", "ja", "zh"].map((from) => ({ from, to: "en" }));
  }

  async loadTranslationModel({ from, to }: { from: string; to: string }) {
    if (!["ar", "et", "ja", "zh"].includes(from) || to !== "en") {
      throw new Error(`Unexpected fixture language pair: ${from}-${to}`);
    }
    const [model, shortlist, vocab0] = await Promise.all(
      ["model.bin", "shortlist.bin", "vocab-0.spm"].map(async (name) => {
        const response = await fetch(`/test-model/${from}/${name}`);
        if (!response.ok) throw new Error(`Could not load ${name}`);
        return response.arrayBuffer();
      }),
    );
    const secondVocab = await fetch(`/test-model/${from}/vocab-1.spm`);
    const vocabs = [vocab0];
    if (secondVocab.ok) vocabs.push(await secondVocab.arrayBuffer());
    return {
      model,
      shortlist,
      vocabs,
      config: {},
    };
  }
}

export async function runOfflineTranslationWorkerIntegration(
  cases: Array<{ source: string; text: string; html?: string }>,
) {
  const translator = new BatchTranslator(
    {
      workers: 1,
      batchSize: 1,
      cacheSize: 0,
      pivotLanguage: null,
    },
    new FixtureTranslationBacking({
      cacheSize: 0,
      pivotLanguage: null,
    }),
  );
  try {
    const results = [];
    for (const { source, text, html: htmlInput } of cases) {
      console.info(`Loading ${source}-to-English Bergamot fixture`);
      const plain = await translator.translate({
        from: source,
        to: "en",
        text,
        html: false,
      });
      const htmlResponse = await translator.translate({
        from: source,
        to: "en",
        text: normalizeHtmlForTranslation(htmlInput ?? `<p>${text}</p>`),
        html: true,
      });
      results.push({
        source,
        input: text,
        plain: plain.target.text,
        html: htmlResponse.target.text,
      });
    }
    return results;
  } finally {
    await translator.delete();
  }
}
