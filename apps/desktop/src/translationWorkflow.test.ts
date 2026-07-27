import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  MailSummary,
  MessageContent,
  TranslationLanguageDetection,
  TranslationModelStatus,
} from "./types";
import {
  buildTranslationDetectionSample,
  translateConversation,
  type TranslationWorkflowDependencies,
} from "./translationWorkflow";

const message = {
  id: "message-1",
  subject: "Project update",
  snippet: "Original preview",
} as MailSummary;
const content: MessageContent = {
  body_text: "Original body",
  attachments: [],
};

const mocks = {
  loadContent: vi.fn(),
  detectLanguage: vi.fn(),
  listModels: vi.fn(),
  approveDownload: vi.fn(),
  installModel: vi.fn(),
  translate: vi.fn(),
};

const dependencies = mocks as unknown as TranslationWorkflowDependencies;

function detection(
  language: string,
  languageName: string,
  reliable = true,
): TranslationLanguageDetection {
  return { language, languageName, reliable };
}

function model(
  source: string,
  sourceName: string,
  installed: boolean,
): TranslationModelStatus {
  return {
    source,
    sourceName,
    target: "en",
    downloadBytes: 42_000_000,
    installed,
  };
}

describe("offline translation workflow integration", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.loadContent.mockResolvedValue(content);
    mocks.listModels.mockResolvedValue([]);
    mocks.approveDownload.mockResolvedValue(true);
    mocks.installModel.mockResolvedValue(undefined);
    mocks.translate.mockImplementation(
      async (_source: string, text: string) => `EN: ${text}`,
    );
  });

  it("routes a detected unsupported language to refusal before model execution", async () => {
    mocks.detectLanguage.mockResolvedValue(detection("eo", "Esperanto", true));

    const result = await translateConversation(
      message.subject,
      [message],
      dependencies,
    );

    expect(result).toEqual({
      kind: "unsupported",
      detection: detection("eo", "Esperanto", true),
    });
    expect(mocks.listModels).toHaveBeenCalledOnce();
    expect(mocks.approveDownload).not.toHaveBeenCalled();
    expect(mocks.installModel).not.toHaveBeenCalled();
    expect(mocks.translate).not.toHaveBeenCalled();
  });

  it("downloads a missing Arabic model and translates only after installation", async () => {
    const arabic = detection("ar", "Arabic");
    const arabicModel = model("ar", "Arabic", false);
    mocks.detectLanguage.mockResolvedValue(arabic);
    mocks.listModels.mockResolvedValue([arabicModel]);
    const progress = vi.fn();

    const result = await translateConversation(
      message.subject,
      [message],
      dependencies,
      progress,
    );

    expect(mocks.approveDownload).toHaveBeenCalledWith(arabicModel, arabic);
    expect(mocks.installModel).toHaveBeenCalledWith("ar", progress);
    expect(mocks.installModel.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.translate.mock.invocationCallOrder[0],
    );
    expect(result).toMatchObject({
      kind: "translated",
      detection: arabic,
      subject: "EN: Project update",
      contents: {
        "message-1": { body_text: "EN: Original body" },
      },
    });
  });

  it("uses an installed Chinese model and preserves the plain fallback for translated HTML", async () => {
    mocks.detectLanguage.mockResolvedValue(detection("zh", "Chinese"));
    mocks.listModels.mockResolvedValue([model("zh", "Chinese", true)]);
    mocks.loadContent.mockResolvedValue({
      ...content,
      body_text: "Plain fallback",
      body_html: "<p>项目更新</p>",
    });

    const result = await translateConversation(
      message.subject,
      [message],
      dependencies,
    );

    expect(mocks.installModel).not.toHaveBeenCalled();
    expect(mocks.translate).toHaveBeenCalledWith("zh", "<p>项目更新</p>", true);
    expect(result).toMatchObject({
      kind: "translated",
      contents: {
        "message-1": {
          body_text: "Plain fallback",
          body_html: "EN: <p>项目更新</p>",
        },
      },
    });
  });

  it("stops a Japanese translation when model download is declined", async () => {
    const japanese = detection("ja", "Japanese");
    mocks.detectLanguage.mockResolvedValue(japanese);
    mocks.listModels.mockResolvedValue([model("ja", "Japanese", false)]);
    mocks.approveDownload.mockResolvedValue(false);

    await expect(
      translateConversation(message.subject, [message], dependencies),
    ).resolves.toEqual({ kind: "cancelled", detection: japanese });
    expect(mocks.installModel).not.toHaveBeenCalled();
    expect(mocks.translate).not.toHaveBeenCalled();
  });

  it("prompts for a model before loading any bodies in a long thread", async () => {
    const messages = Array.from({ length: 250 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
      subject: "مطالبة تأمين",
      snippet: "يرجى مراجعة تفاصيل المطالبة والمستندات المرفقة.",
    }));
    const arabic = detection("ar", "Arabic");
    const arabicModel = model("ar", "Arabic", false);
    mocks.detectLanguage.mockResolvedValue(arabic);
    mocks.listModels.mockResolvedValue([arabicModel]);
    mocks.approveDownload.mockResolvedValue(false);

    await expect(
      translateConversation("مطالبة تأمين", messages, dependencies),
    ).resolves.toEqual({ kind: "cancelled", detection: arabic });

    expect(mocks.approveDownload).toHaveBeenCalledWith(arabicModel, arabic);
    expect(mocks.loadContent).not.toHaveBeenCalled();
  });

  it("bounds preview detection and prioritizes the newest messages", () => {
    const messages = Array.from({ length: 500 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
      subject: `Subject ${index}`,
      snippet: `Preview ${index} ${"x".repeat(200)}`,
    }));

    const sample = buildTranslationDetectionSample("Primary", messages);

    expect(sample.length).toBeLessThanOrEqual(10_000);
    expect(sample).toContain("Preview 499");
    expect(sample).not.toContain("Preview 0 ");
  });

  it("translates large conversations with one in-flight WASM request", async () => {
    const messages = Array.from({ length: 100 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
      subject: `Uuendus ${index}`,
      snippet: "See on eestikeelne vestlus.",
    }));
    mocks.detectLanguage.mockResolvedValue(detection("et", "Estonian"));
    mocks.listModels.mockResolvedValue([model("et", "Estonian", true)]);
    mocks.loadContent.mockImplementation(async (messageId: string) => ({
      body_text: `Sisu ${messageId}`,
      attachments: [],
    }));
    let activeTranslations = 0;
    let maximumActiveTranslations = 0;
    mocks.translate.mockImplementation(
      async (_source: string, text: string) => {
        activeTranslations += 1;
        maximumActiveTranslations = Math.max(
          maximumActiveTranslations,
          activeTranslations,
        );
        await Promise.resolve();
        activeTranslations -= 1;
        return `EN: ${text}`;
      },
    );

    const result = await translateConversation(
      "Pikk vestlus",
      messages,
      dependencies,
    );

    expect(result.kind).toBe("translated");
    expect(mocks.translate).toHaveBeenCalledTimes(101);
    expect(maximumActiveTranslations).toBe(1);
  });

  it("stops after a body fails instead of returning a partial conversation", async () => {
    const messages = Array.from({ length: 3 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
    }));
    mocks.detectLanguage.mockResolvedValue(detection("et", "Estonian"));
    mocks.listModels.mockResolvedValue([model("et", "Estonian", true)]);
    mocks.translate
      .mockResolvedValueOnce("English subject")
      .mockResolvedValueOnce("English first body")
      .mockRejectedValueOnce(new Error("Worker stopped"));

    await expect(
      translateConversation("Vestlus", messages, dependencies),
    ).rejects.toThrow("Worker stopped");
    expect(mocks.translate).toHaveBeenCalledTimes(3);
  });

  it("does not query models when detection is English or unreliable", async () => {
    mocks.detectLanguage.mockResolvedValueOnce(detection("en", "English"));
    await expect(
      translateConversation(message.subject, [message], dependencies),
    ).resolves.toEqual({
      kind: "already-english",
      detection: detection("en", "English"),
    });

    mocks.detectLanguage.mockResolvedValue(
      detection("und", "Unknown language", false),
    );
    await expect(
      translateConversation(message.subject, [message], dependencies),
    ).resolves.toEqual({
      kind: "unsupported",
      detection: detection("und", "Unknown language", false),
    });
    expect(mocks.loadContent).toHaveBeenCalledOnce();
    expect(mocks.detectLanguage).toHaveBeenCalledTimes(3);
    expect(mocks.listModels).not.toHaveBeenCalled();
  });
});
