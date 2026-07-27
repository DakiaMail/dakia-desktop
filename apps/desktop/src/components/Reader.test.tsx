import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { MailSummary } from "../types";
import { Reader } from "./Reader";

const translationMocks = vi.hoisted(() => ({
  detect: vi.fn(),
  translate: vi.fn(),
  confirm: vi.fn(),
}));

vi.mock("../offlineTranslation", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../offlineTranslation")>()),
  detectTranslationLanguage: translationMocks.detect,
  translateOffline: translationMocks.translate,
}));

vi.mock("../nativeFeedback", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../nativeFeedback")>()),
  confirmNativeAction: translationMocks.confirm,
}));

const message: MailSummary = {
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 1,
  thread_id: "thread-1",
  subject: "Weekly notes",
  from_address: "list@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Message body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
};

const props = {
  aiLoading: false,
  aiConnected: false,
  actionsDisabled: false,
  onArchive: vi.fn(),
  onSpam: vi.fn(),
  onTrash: vi.fn(),
  onReply: vi.fn(),
  onForward: vi.fn(),
  onToggleRead: vi.fn(),
  onSummarize: vi.fn(),
  onCopyAi: vi.fn(),
  unsubscribeLoading: false,
  onUnsubscribe: vi.fn(),
  onToggleStar: vi.fn(),
};

describe("Reader unsubscribe action", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    translationMocks.detect.mockResolvedValue({
      language: "et",
      languageName: "Estonian",
      reliable: true,
    });
    translationMocks.translate.mockImplementation(
      async (_source: string, text: string) => `EN: ${text}`,
    );
    translationMocks.confirm.mockResolvedValue(true);
    vi.spyOn(api, "content").mockResolvedValue({
      body_text: "Tere maailm",
      attachments: [],
    });
    vi.spyOn(api, "translationModels").mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: true,
      },
    ]);
    vi.spyOn(api, "installTranslationModel").mockResolvedValue({
      source: "et",
      target: "en",
      modelPath: "/model",
      shortlistPath: "/shortlist",
      vocabPaths: ["/vocab"],
      config: {},
    });
    vi.spyOn(api, "cancelTranslationModelInstall").mockResolvedValue();
  });

  it("keeps AI controls and results hidden even when connected", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={message}
          aiConnected
          aiResult="Hidden summary"
        />
      </MantineProvider>,
    );

    expect(screen.queryByRole("button", { name: "Summarize" })).toBeNull();
    expect(screen.queryByText("Hidden summary")).toBeNull();
  });

  it("shows unsubscribe only for a supported message", () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    expect(screen.queryByRole("button", { name: "Unsubscribe" })).toBeNull();

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, unsubscribe_kind: "one_click" }}
        />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    expect(props.onUnsubscribe).toHaveBeenCalledOnce();
  });

  it("disables unsubscribe while the request is in progress", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          unsubscribeLoading
          message={{ ...message, unsubscribe_kind: "one_click" }}
        />
      </MantineProvider>,
    );
    expect(screen.getByRole("button", { name: "Unsubscribe" })).toBeDisabled();
  });

  it("marks messages sent by the active account", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          accountEmail="me@example.com"
          message={{
            ...message,
            mailbox: "Sent",
            from_address: "me@example.com",
          }}
        />
      </MantineProvider>,
    );
    expect(screen.getByText("Sent by you")).toBeVisible();
  });

  it("shows a localized loading state for header-first arrivals", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, body_text: "", content_state: "headers_only" }}
        />
      </MantineProvider>,
    );
    expect(screen.getByRole("status")).toHaveTextContent(
      "Loading the full message",
    );
  });

  it("offers forwarding from the message actions menu", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Forward" }));
    expect(props.onForward).toHaveBeenCalledOnce();
    expect(props.onForward.mock.calls[0]).toEqual([]);
  });

  it("runs archive from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    expect(props.onArchive).toHaveBeenCalledOnce();
  });

  it("opens reply from the latest message header action", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} messages={[message]} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Reply" }));
    expect(props.onReply).toHaveBeenCalledOnce();
    expect(props.onReply.mock.calls[0]).toEqual([]);
  });

  it("runs delete from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(props.onTrash).toHaveBeenCalledOnce();
  });

  it("offers delete from the message actions menu", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Delete" }));
    expect(props.onTrash).toHaveBeenCalledOnce();
  });

  it("switches the toolbar action to Not spam for spam messages", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, mailbox: "Spam" }} />
      </MantineProvider>,
    );
    expect(screen.getByRole("button", { name: "Not spam" })).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Mark as spam" }),
    ).not.toBeInTheDocument();
  });

  it("toggles the star action from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Star conversation" }));
    expect(props.onToggleStar).toHaveBeenCalledWith(
      expect.objectContaining({ id: "message-1" }),
      true,
    );
  });

  it("toggles the reader read action for unread and read conversations", async () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Mark as read" }));
    expect(props.onToggleRead).toHaveBeenCalledWith(true);

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, id: "message-2", is_read: true }}
        />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Mark as unread" }));
    expect(props.onToggleRead).toHaveBeenCalledWith(false);
  });

  it("offers offline translation even when AI is disconnected", () => {
    render(
      <MantineProvider>
        <Reader {...props} aiConnected={false} message={message} />
      </MantineProvider>,
    );

    expect(
      screen.getByRole("button", { name: "Translate to English" }),
    ).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Summarize" }),
    ).not.toBeInTheDocument();
  });

  it("translates the subject and plain-text body, then restores the original", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    expect(
      await screen.findByText("Translated from Estonian on this device"),
    ).toBeVisible();
    expect(
      screen.getByRole("heading", { name: "EN: Weekly notes" }),
    ).toBeVisible();
    expect(screen.getByText("EN: Tere maailm")).toBeVisible();
    expect(translationMocks.translate).toHaveBeenNthCalledWith(
      1,
      "et",
      "Weekly notes",
      false,
    );
    expect(translationMocks.translate).toHaveBeenNthCalledWith(
      2,
      "et",
      "Tere maailm",
      false,
    );

    fireEvent.click(screen.getByRole("button", { name: "Show original" }));
    expect(screen.getByRole("heading", { name: "Weekly notes" })).toBeVisible();
    expect(screen.getByText("Tere maailm")).toBeVisible();
  });

  it("ignores stale translation results after switching conversations", async () => {
    let finishSubject: ((value: string) => void) | undefined;
    translationMocks.translate.mockImplementationOnce(
      () =>
        new Promise<string>((resolve) => {
          finishSubject = resolve;
        }),
    );
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    await waitFor(() => expect(translationMocks.translate).toHaveBeenCalled());

    const nextMessage = {
      ...message,
      id: "message-next",
      thread_id: "thread-next",
      subject: "Different conversation",
    };
    rerender(
      <MantineProvider>
        <Reader {...props} message={nextMessage} />
      </MantineProvider>,
    );
    finishSubject?.("Old translated subject");

    await waitFor(() =>
      expect(translationMocks.translate).toHaveBeenCalledTimes(2),
    );
    expect(
      screen.getByRole("heading", { name: "Different conversation" }),
    ).toBeVisible();
    expect(
      screen.queryByText("Translated from Estonian on this device"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText("Old translated subject"),
    ).not.toBeInTheDocument();
  });

  it("translates every message in the active conversation", async () => {
    const earlier = {
      ...message,
      id: "message-0",
      uid: 0,
      body_text: "Varasem kiri",
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text: id === earlier.id ? "Esimene kiri" : "Teine kiri",
      attachments: [],
    }));
    render(
      <MantineProvider>
        <Reader {...props} message={message} messages={[earlier, message]} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    expect(await screen.findByText("EN: Esimene kiri")).toBeVisible();
    expect(screen.getByText("EN: Teine kiri")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(2);
    expect(api.content).toHaveBeenCalledWith("message-0");
    expect(api.content).toHaveBeenCalledWith("message-1");
    expect(translationMocks.translate).toHaveBeenCalledWith(
      "et",
      "Esimene kiri",
      false,
    );
    expect(translationMocks.translate).toHaveBeenCalledWith(
      "et",
      "Teine kiri",
      false,
    );
  });

  it("translates HTML as HTML and keeps rendering through the sanitized email iframe", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Tere",
      body_html: "<p>Tere <strong>maailm</strong></p>",
      attachments: [],
    });
    translationMocks.translate
      .mockResolvedValueOnce("English subject")
      .mockResolvedValueOnce("<p>Hello <strong>world</strong></p>");
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    const frame = await screen.findByTitle("Weekly notes");
    expect(translationMocks.translate).toHaveBeenLastCalledWith(
      "et",
      "<p>Tere <strong>maailm</strong></p>",
      true,
    );
    expect(frame).toHaveAttribute(
      "srcdoc",
      expect.stringContaining("<strong>world</strong>"),
    );
    expect(frame).toHaveAttribute(
      "srcdoc",
      expect.stringContaining("Content-Security-Policy"),
    );
  });

  it("downloads a missing pack with progress before translating", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    vi.mocked(api.installTranslationModel).mockImplementation(
      async (_source, onProgress) => {
        onProgress?.({
          source: "et",
          downloadedBytes: 10_500_000,
          totalBytes: 21_000_000,
          fileIndex: 1,
          fileCount: 3,
        });
        await Promise.resolve();
        return {
          source: "et",
          target: "en",
          modelPath: "/model",
          shortlistPath: "/shortlist",
          vocabPaths: ["/vocab"],
          config: {},
        };
      },
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(translationMocks.confirm.mock.calls[0][0]).toBe(
      "Download Estonian translation?",
    );
    expect(translationMocks.confirm.mock.calls[0][1]).toContain(
      "without an internet connection",
    );
    await waitFor(() =>
      expect(api.installTranslationModel).toHaveBeenCalledWith(
        "et",
        expect.any(Function),
      ),
    );
    expect(
      await screen.findByText("Translated from Estonian on this device"),
    ).toBeVisible();
  });

  it("shows the model prompt without waiting for remote message content", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    vi.mocked(api.content).mockReturnValue(new Promise(() => undefined));
    translationMocks.confirm.mockResolvedValue(false);
    const longThread = Array.from({ length: 50 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
      uid: index,
      snippet: "Tere, palun vaadake kahjuteate üksikasju.",
    }));

    render(
      <MantineProvider>
        <Reader {...props} message={longThread.at(-1)} messages={longThread} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(api.installTranslationModel).not.toHaveBeenCalled();
  });

  it("does not download when the user declines the language pack", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    translationMocks.confirm.mockResolvedValue(false);
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(api.installTranslationModel).not.toHaveBeenCalled();
    expect(translationMocks.translate).not.toHaveBeenCalled();
  });

  it("cancels an in-progress pack download through the native backend", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    let finishDownload:
      | ((
          value: Awaited<ReturnType<typeof api.installTranslationModel>>,
        ) => void)
      | undefined;
    vi.mocked(api.installTranslationModel).mockImplementation(
      (_source, onProgress) =>
        new Promise((resolve) => {
          finishDownload = resolve;
          onProgress?.({
            source: "et",
            downloadedBytes: 1_000,
            totalBytes: 21_000_000,
            fileIndex: 1,
            fileCount: 3,
          });
        }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));

    expect(api.cancelTranslationModelInstall).toHaveBeenCalledWith("et");
    finishDownload?.({
      source: "et",
      target: "en",
      modelPath: "/model",
      shortlistPath: "/shortlist",
      vocabPaths: ["/vocab"],
      config: {},
    });
  });

  it("handles English, unsupported, and backend errors without exposing raw failures", async () => {
    translationMocks.detect.mockResolvedValueOnce({
      language: "en",
      languageName: "English",
      reliable: true,
    });
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText("This conversation is already in English."),
    ).toBeVisible();
    expect(api.translationModels).not.toHaveBeenCalled();

    translationMocks.detect.mockResolvedValueOnce({
      language: "ja",
      languageName: "Japanese",
      reliable: true,
    });
    rerender(
      <MantineProvider>
        <Reader {...props} message={{ ...message, id: "message-2" }} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText(
        /Offline translation from Japanese is not available/,
      ),
    ).toBeVisible();

    const consoleError = vi
      .spyOn(console, "error")
      .mockImplementation(() => undefined);
    translationMocks.translate.mockRejectedValueOnce(
      new Error("Aborted(). Build with -s ASSERTIONS=1 for more info."),
    );
    rerender(
      <MantineProvider>
        <Reader {...props} message={{ ...message, id: "message-3" }} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText("Offline translation failed."),
    ).toBeVisible();
    expect(screen.queryByText(/Aborted|ASSERTIONS=1/)).not.toBeInTheDocument();
    expect(consoleError).toHaveBeenCalled();
    consoleError.mockRestore();
  });
});
