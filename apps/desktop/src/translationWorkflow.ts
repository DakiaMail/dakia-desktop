import type {
  MailSummary,
  MessageContent,
  TranslationDownloadProgress,
  TranslationLanguageDetection,
  TranslationModelStatus,
} from "./types";

export type TranslationWorkflowDependencies = {
  loadContent: (messageId: string) => Promise<MessageContent>;
  detectLanguage: (text: string) => Promise<TranslationLanguageDetection>;
  listModels: () => Promise<TranslationModelStatus[]>;
  approveDownload: (
    model: TranslationModelStatus,
    detection: TranslationLanguageDetection,
  ) => Promise<boolean>;
  installModel: (
    source: string,
    onProgress?: (progress: TranslationDownloadProgress) => void,
  ) => Promise<unknown>;
  translate: (source: string, text: string, html?: boolean) => Promise<string>;
};

type TranslationOriginal = {
  item: MailSummary;
  content: MessageContent;
};

const DETECTION_SAMPLE_LIMIT = 10_000;

export function buildTranslationDetectionSample(
  primarySubject: string,
  messages: MailSummary[],
) {
  const parts = [primarySubject];
  let length = primarySubject.length;
  for (const item of [...messages].reverse()) {
    if (length >= DETECTION_SAMPLE_LIMIT) break;
    const preview = `${item.subject}\n${item.snippet || item.body_text}`;
    parts.push(preview.slice(0, DETECTION_SAMPLE_LIMIT - length));
    length += preview.length;
  }
  return parts.join("\n\n").slice(0, DETECTION_SAMPLE_LIMIT);
}

export type TranslationWorkflowResult =
  | {
      kind: "already-english";
      detection: TranslationLanguageDetection;
    }
  | {
      kind: "unsupported";
      detection: TranslationLanguageDetection;
    }
  | {
      kind: "cancelled";
      detection: TranslationLanguageDetection;
    }
  | {
      kind: "translated";
      detection: TranslationLanguageDetection;
      subject: string;
      contents: Record<string, MessageContent>;
    };

export async function translateConversation(
  primarySubject: string,
  messages: MailSummary[],
  dependencies: TranslationWorkflowDependencies,
  onProgress?: (progress: TranslationDownloadProgress) => void,
): Promise<TranslationWorkflowResult> {
  let detection = await dependencies.detectLanguage(
    buildTranslationDetectionSample(primarySubject, messages),
  );
  let detectionContent: TranslationOriginal | undefined;
  if (!detection.reliable && messages.length) {
    const item = messages.at(-1)!;
    detectionContent = {
      item,
      content: await dependencies.loadContent(item.id),
    };
    detection = await dependencies.detectLanguage(
      `${item.subject}\n${detectionContent.content.body_text}`,
    );
  }
  if (detection.language === "en") {
    return { kind: "already-english", detection };
  }
  if (!detection.reliable) {
    return { kind: "unsupported", detection };
  }

  const models = await dependencies.listModels();
  const model = models.find(
    (candidate) =>
      candidate.source === detection.language && candidate.target === "en",
  );
  if (!model) {
    return { kind: "unsupported", detection };
  }

  const loadOriginals = () =>
    Promise.all(
      messages.map(async (item) => ({
        item,
        content:
          detectionContent?.item.id === item.id
            ? detectionContent.content
            : await dependencies.loadContent(item.id),
      })),
    );

  let originals: TranslationOriginal[];
  if (!model.installed) {
    if (!(await dependencies.approveDownload(model, detection))) {
      return { kind: "cancelled", detection };
    }
    [, originals] = await Promise.all([
      dependencies.installModel(model.source, onProgress),
      loadOriginals(),
    ]);
  } else {
    originals = await loadOriginals();
  }

  const subject = await dependencies.translate(
    model.source,
    primarySubject,
    false,
  );
  const translatedBodies: string[] = [];
  for (const { content } of originals) {
    translatedBodies.push(
      await dependencies.translate(
        model.source,
        content.body_html ?? content.body_text,
        Boolean(content.body_html),
      ),
    );
  }
  return {
    kind: "translated",
    detection,
    subject,
    contents: Object.fromEntries(
      originals.map(({ item, content }, index) => [
        item.id,
        {
          ...content,
          body_text: content.body_html
            ? content.body_text
            : translatedBodies[index],
          body_html: content.body_html ? translatedBodies[index] : undefined,
        },
      ]),
    ),
  };
}
