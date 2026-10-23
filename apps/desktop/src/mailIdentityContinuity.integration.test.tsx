import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import App from "./App";
import { ComposeApp } from "./ComposeApp";
import { ReaderWindowApp } from "./ReaderWindowApp";
import type { ComposeSeed } from "./composeWindow";
import type { Account, MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => {
  class SendMessageError extends Error {
    constructor(
      readonly kind:
        "not_submitted" | "delivery_uncertain" | "sent_copy_failed",
    ) {
      super("safe send failure");
    }
  }
  return {
    SendMessageError,
    api: {
      accounts: vi.fn(),
      action: vi.fn(),
      aiAvailable: vi.fn(),
      classifyPending: vi.fn(),
      configureTray: vi.fn(),
      content: vi.fn(),
      conversationForTarget: vi.fn(),
      draft: vi.fn(),
      forwardAttachments: vi.fn(),
      mailRebuildStatus: vi.fn(),
      openExternal: vi.fn(),
      readDroppedFiles: vi.fn(),
      recordNotificationDelivered: vi.fn(),
      search: vi.fn(),
      searchRemote: vi.fn(),
      send: vi.fn(),
      setCategory: vi.fn(),
      setRead: vi.fn(),
      setStarred: vi.fn(),
      showEmailAddressContextMenu: vi.fn(),
      smartInbox: vi.fn(),
      starredCount: vi.fn(),
      startRealtimeSync: vi.fn(),
      summarize: vi.fn(),
      sync: vi.fn(),
      trashMessagesFromSender: vi.fn(),
      unsubscribe: vi.fn(),
    },
    showNativeMessage: vi.fn(),
    confirmNativeAction: vi.fn(),
    checkForUpdate: vi.fn(),
    invoke: vi.fn(),
    openedUrls: [] as string[],
  };
});
const handoffEvents = vi.hoisted(() => ({
  emitTo: vi.fn(),
  composeReady: undefined as
    | ((event: { payload: { label: string; seedToken: string } }) => void)
    | undefined,
  listen: vi.fn(),
}));

vi.mock("./api", () => ({
  api: mocks.api,
  SendMessageError: mocks.SendMessageError,
}));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: mocks.confirmNativeAction,
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./notifications", () => ({
  onNotificationAction: vi.fn(async () => () => undefined),
  readNotificationSettings: () => ({
    enabled: false,
    soundEnabled: false,
    showPreview: false,
  }),
  requestInitialNotificationAccess: vi.fn(async () => undefined),
  sendNewMailNotification: vi.fn(async () => false),
}));
vi.mock("./updater", () => ({
  checkForUpdate: mocks.checkForUpdate,
  downloadUpdate: vi.fn(async () => undefined),
  installUpdateAndRelaunch: vi.fn(async () => undefined),
}));
vi.mock("./analytics", () => ({
  readAnalyticsSettings: () => ({ consent: "disabled" }),
  reportUsageIfConsented: vi.fn(async () => false),
  listenForAnalyticsConsent: vi.fn(async () => () => undefined),
  setAnalyticsConsent: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => handoffEvents);
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    close: vi.fn(async () => undefined),
    isFocused: vi.fn(async () => true),
    setFocus: vi.fn(async () => undefined),
    show: vi.fn(async () => undefined),
    startDragging: vi.fn(async () => undefined),
  }),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    listen: vi.fn(async () => () => undefined),
  }),
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getAllWebviewWindows: vi.fn(async () => []),
}));

const firstAccount: Account = {
  id: "account-1",
  email: "first@example.test",
  account_name: "First inbox",
  display_name: "First User",
  provider_id: "fastmail",
  auth: { type: "password", username: "first@example.test" },
  imap_host: "imap.example.test",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.example.test",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};

const secondAccount: Account = {
  ...firstAccount,
  id: "account-2",
  email: "second@example.test",
  account_name: "Second inbox",
  display_name: "Second User",
  auth: { type: "password", username: "second@example.test" },
};

const searchedMessage: MailSummary = {
  id: "searched-message",
  account_id: secondAccount.id,
  mailbox: "INBOX",
  uid: 42,
  mailbox_uid_validity: 90210,
  message_id: "<searched-message@example.test>",
  thread_id: "searched-thread",
  subject: "Search result with attachment",
  from_name: "Fictional Sender",
  from_address: "sender@example.test",
  to_addresses: secondAccount.email,
  received_at: "2026-10-18T08:00:00Z",
  snippet: "Fictional search result",
  body_text: "Fictional provider body",
  is_read: true,
  is_flagged: false,
  has_attachments: true,
};

const searchedThread: MailThread = {
  id: `${secondAccount.id}:${searchedMessage.thread_id}`,
  accountId: secondAccount.id,
  threadId: searchedMessage.thread_id,
  messages: [searchedMessage],
  sourceMessages: [searchedMessage],
  latest: searchedMessage,
  messageCount: 1,
  unread: false,
  hasAttachments: true,
  participants: [searchedMessage.from_address],
};

const firstAccountCollisionMessage: MailSummary = {
  ...searchedMessage,
  id: "first-account-colliding-message",
  account_id: firstAccount.id,
  subject: "First account collision",
  to_addresses: firstAccount.email,
};

const firstAccountCollisionThread: MailThread = {
  ...searchedThread,
  id: `${firstAccount.id}:${firstAccountCollisionMessage.thread_id}`,
  accountId: firstAccount.id,
  messages: [firstAccountCollisionMessage],
  sourceMessages: [firstAccountCollisionMessage],
  latest: firstAccountCollisionMessage,
};

const replyAllMessage: MailSummary = {
  ...searchedMessage,
  id: "reply-all-message",
  uid: 314,
  message_id: "<reply-all-message@example.test>",
  thread_id: "reply-all-thread",
  subject: "Reply all identity",
  from_name: "Original Sender",
  from_address: "sender@example.test",
  to_addresses:
    "Second User <second@example.test>, Peer Person <peer@example.test>, PEER@example.test, Reply Desk <reply@example.test>",
  cc_addresses:
    "peer@example.test, Other Person <other@example.test>, OTHER@example.test, Second User <second@example.test>",
  bcc_addresses: "Hidden Recipient <hidden@example.test>",
  reply_to_addresses: "Reply Desk <reply@example.test>",
  reference_ids: "<reply-root@example.test> <reply-parent@example.test>",
  body_text: "Reply all preview body",
  has_attachments: false,
};

const replyAllThread: MailThread = {
  id: `${secondAccount.id}:${replyAllMessage.thread_id}`,
  accountId: secondAccount.id,
  threadId: replyAllMessage.thread_id,
  messages: [replyAllMessage],
  sourceMessages: [replyAllMessage],
  latest: replyAllMessage,
  messageCount: 1,
  unread: false,
  hasAttachments: false,
  participants: [replyAllMessage.from_address],
};

function emailRendererFrame(host: HTMLElement | null | undefined) {
  return host?.shadowRoot?.querySelector("iframe") ?? null;
}

function emailRendererDocument(host: HTMLElement | null | undefined) {
  const source = emailRendererFrame(host)?.getAttribute("srcdoc");
  return source
    ? new DOMParser().parseFromString(source, "text/html").documentElement
    : null;
}

function emailRendererToken(host: HTMLElement | null | undefined) {
  const token = emailRendererDocument(host)
    ?.querySelector("script[data-dakia-renderer-token]")
    ?.getAttribute("data-dakia-renderer-token");
  if (!token) throw new Error("Expected sandbox renderer token");
  return token;
}

function sendEmailRendererMessage(
  host: HTMLElement | null | undefined,
  data: Record<string, unknown>,
) {
  const frame = emailRendererFrame(host);
  if (!frame) throw new Error("Expected sandbox renderer frame");
  if (!frame.contentWindow) {
    Object.defineProperty(frame, "contentWindow", {
      configurable: true,
      value: window,
    });
  }
  window.dispatchEvent(
    new MessageEvent("message", { data, source: frame.contentWindow }),
  );
}

function installComposeSeedIndexedDb() {
  const records = new Map<string, unknown>();
  const database = {
    objectStoreNames: { contains: () => true },
    close: vi.fn(),
    createObjectStore: vi.fn(),
    transaction: vi.fn(() => {
      const transaction: {
        oncomplete?: () => void;
        onerror?: () => void;
        onabort?: () => void;
        error?: DOMException | null;
        objectStore: () => {
          get: (key: string) => IDBRequest;
          put: (value: unknown, key: string) => void;
          delete: (key: string) => void;
        };
      } = {
        objectStore: () => ({
          get: (key) => {
            const request: Partial<IDBRequest> = {};
            queueMicrotask(() => {
              Object.assign(request, { result: records.get(key) });
              (request.onsuccess as ((event: Event) => void) | null)?.(
                new Event("success"),
              );
              transaction.oncomplete?.();
            });
            return request as IDBRequest;
          },
          put: (value, key) => {
            queueMicrotask(() => {
              records.set(key, value);
              transaction.oncomplete?.();
            });
          },
          delete: (key) => {
            queueMicrotask(() => {
              records.delete(key);
              transaction.oncomplete?.();
            });
          },
        }),
      };
      return transaction as unknown as IDBTransaction;
    }),
  };
  Object.defineProperty(globalThis, "indexedDB", {
    configurable: true,
    value: {
      open: vi.fn(() => {
        const request: Partial<IDBOpenDBRequest> = {
          result: database as unknown as IDBDatabase,
        };
        queueMicrotask(() => {
          (request.onsuccess as ((event: Event) => void) | null)?.(
            new Event("success"),
          );
        });
        return request as IDBOpenDBRequest;
      }),
    },
  });
  return records;
}

describe("search to detached Reader to compose identity continuity", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.openedUrls.length = 0;
    localStorage.clear();
    sessionStorage.clear();
    window.history.replaceState({}, "", "/");
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    vi.spyOn(window, "open").mockImplementation((url) => {
      mocks.openedUrls.push(String(url));
      return window;
    });
    vi.spyOn(window, "close").mockImplementation(() => undefined);

    mocks.api.accounts.mockResolvedValue([firstAccount, secondAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0] ? [searchedThread] : [],
      nextCursor: null,
    }));
    mocks.api.searchRemote.mockResolvedValue({
      messages: [searchedMessage],
      limitReached: false,
    });
    mocks.api.smartInbox.mockResolvedValue({ sections: [] });
    mocks.api.starredCount.mockResolvedValue(0);
    mocks.api.classifyPending.mockResolvedValue(0);
    mocks.api.mailRebuildStatus.mockResolvedValue([]);
    mocks.api.openExternal.mockResolvedValue(undefined);
    mocks.api.configureTray.mockResolvedValue(undefined);
    mocks.api.startRealtimeSync.mockResolvedValue(undefined);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.conversationForTarget.mockResolvedValue(searchedThread);
    mocks.api.content.mockResolvedValue({
      body_text: "Full fictional provider body",
      body_html: "<p>Full fictional provider body</p>",
      attachments: [
        {
          id: "attachment-metadata",
          message_id: searchedMessage.id,
          filename: "fictional-report.pdf",
          mime_type: "application/pdf",
          size_bytes: 3,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
      ],
    });
    mocks.api.forwardAttachments.mockResolvedValue([
      {
        filename: "fictional-report.pdf",
        mime_type: "application/pdf",
        size_bytes: 3,
        content_base64: "cGRm",
      },
    ]);
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.api.setStarred.mockResolvedValue(searchedMessage);
    mocks.api.action.mockResolvedValue(undefined);
    mocks.api.readDroppedFiles.mockResolvedValue([]);
    mocks.api.send.mockResolvedValue("queued");
    mocks.api.draft.mockResolvedValue("");
    mocks.api.summarize.mockResolvedValue("");
    mocks.api.unsubscribe.mockResolvedValue({
      kind: "completed",
      cleanupTarget: null,
    });
    mocks.api.trashMessagesFromSender.mockResolvedValue({
      matched: 0,
      moved: 0,
      failed: 0,
    });
    mocks.checkForUpdate.mockResolvedValue(null);
    handoffEvents.listen.mockImplementation(async (event, handler) => {
      if (event === "compose-ready") handoffEvents.composeReady = handler;
      return () => undefined;
    });
    mocks.invoke.mockImplementation(async (command, args) => {
      if (command !== "open_compose_window") return undefined;
      const label = "compose-11111111-1111-4111-8111-111111111111";
      handoffEvents.composeReady?.({
        payload: {
          label,
          seedToken: (args as { seedDbKey: string }).seedDbKey,
        },
      });
      return label;
    });
  });

  async function openMailtoThroughDetachedReader(href: string) {
    const escapedHref = href.replaceAll("&", "&amp;").replaceAll('"', "&quot;");
    mocks.api.content.mockResolvedValue({
      body_text: "Open the fictional compose link.",
      body_html: `<p><a href="${escapedHref}">Compose fictional mail</a></p>`,
      attachments: [],
    });

    const main = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "fictional mailto" } },
    );
    fireEvent.doubleClick(
      (await screen.findByText(searchedMessage.subject)).closest("button")!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    const readerUrl = mocks.openedUrls[0];
    main.unmount();
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    const renderedMessage = await screen.findByRole("document", {
      name: searchedMessage.subject,
    });
    const renderedAnchor = emailRendererDocument(
      renderedMessage,
    )?.querySelector("a[data-dakia-external-href]");
    expect(renderedAnchor).not.toBeNull();
    const routedHref = renderedAnchor?.getAttribute("data-dakia-external-href");
    expect(routedHref).toBe(href);
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    sendEmailRendererMessage(renderedMessage, {
      channel: "dakia-email-renderer",
      token: emailRendererToken(renderedMessage),
      type: "external-link",
      href: routedHref,
    });

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith("open_compose_window", {
        seedKey: null,
        seedDbKey: expect.any(String),
      }),
    );
    const composeToken = mocks.invoke.mock.calls.find(
      ([command]) => command === "open_compose_window",
    )?.[1]?.seedDbKey;
    expect(composeToken).toEqual(expect.any(String));
    const composeUrl = `/?view=compose&seedDbKey=${composeToken}`;
    expect(composeUrl).not.toContain(secondAccount.id);
    expect(composeUrl).not.toContain(searchedMessage.id);
    expect(composeUrl).not.toContain("example.test");
    const target = handoffEvents.emitTo.mock.calls.find(
      ([label, event]) =>
        label === "compose-11111111-1111-4111-8111-111111111111" &&
        event === "compose-target",
    )?.[2] as { seed?: ComposeSeed } | undefined;
    const storedSeed = (target?.seed ?? {}) as ComposeSeed &
      Record<string, unknown>;
    // Model the Compose child after it accepts the private target event. The
    // opener's storage remains intentionally empty.
    const composeSeeds = installComposeSeedIndexedDb();
    composeSeeds.set(composeToken, storedSeed);
    sessionStorage.setItem(
      "dakia.compose-active-seed",
      JSON.stringify(storedSeed),
    );
    expect(
      localStorage.getItem(`dakia.compose-seed.${composeToken}`),
    ).toBeNull();

    reader.unmount();
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    window.history.replaceState({}, "", composeUrl);
    const compose = render(<ComposeApp />);
    expect(await screen.findByLabelText("From")).toHaveValue(secondAccount.id);
    expect(mocks.api.send).not.toHaveBeenCalled();
    expect(mocks.api.openExternal).not.toHaveBeenCalled();
    return { compose, storedSeed };
  }

  it("keeps the searched account and mailbox epoch through Reader forwarding and attachment removal", async () => {
    const main = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "fictional report" } },
    );
    const result = await screen.findByText("Search result with attachment");
    fireEvent.doubleClick(result.closest("button")!);

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    const readerUrl = mocks.openedUrls[0];
    expect(readerUrl).toMatch(/^\/\?view=reader&seedKey=/);
    expect(readerUrl).not.toContain(secondAccount.id);
    expect(readerUrl).not.toContain(searchedMessage.id);
    main.unmount();
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
        accountId: secondAccount.id,
        localMessageId: searchedMessage.id,
        rfcMessageId: searchedMessage.message_id,
        threadId: searchedMessage.thread_id,
        mailbox: "INBOX",
      }),
    );
    const forwardButtons = await screen.findAllByRole("button", {
      name: "Forward",
    });
    fireEvent.click(forwardButtons.at(-1)!);

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(2));
    expect(mocks.api.content).toHaveBeenCalledWith(
      searchedMessage.id,
      searchedMessage.mailbox_uid_validity,
    );
    const composeUrl = mocks.openedUrls[1];
    expect(composeUrl).toMatch(/^\/\?view=compose&seedKey=/);
    expect(composeUrl).not.toContain(secondAccount.id);
    expect(composeUrl).not.toContain(searchedMessage.id);
    reader.unmount();
    window.history.replaceState({}, "", composeUrl);

    render(<ComposeApp />);
    expect(await screen.findByText("fictional-report.pdf")).toBeVisible();
    expect(screen.getByLabelText("From")).toHaveValue(secondAccount.id);
    expect(screen.getByLabelText("Subject")).toHaveValue(
      "Fwd: Search result with attachment",
    );
    expect(mocks.api.forwardAttachments).toHaveBeenCalledWith(
      searchedMessage.id,
      searchedMessage.mailbox_uid_validity,
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "Remove fictional-report.pdf",
      }),
    );
    expect(screen.queryByText("fictional-report.pdf")).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "reviewer@example.test" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: secondAccount.id,
        subject: "Fwd: Search result with attachment",
        attachments: [],
        send_attempt_id: expect.any(String),
      }),
    );
  });

  it("keeps one chosen forward attachment with the exact searched account and mailbox epoch", async () => {
    const keptAttachment = {
      filename: "keep-account-two.txt",
      mime_type: "text/plain",
      size_bytes: 25,
      content_base64: "YWNjb3VudC10d28ta2VwdC1ieXRlcw==",
    };
    const removedAttachment = {
      filename: "remove-account-two.pdf",
      mime_type: "application/pdf",
      size_bytes: 27,
      content_base64: "YWNjb3VudC10d28tcmVtb3ZlZC1ieXRlcw==",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0]
        ? [firstAccountCollisionThread, searchedThread]
        : [],
      nextCursor: null,
    }));
    mocks.api.searchRemote.mockResolvedValue([
      firstAccountCollisionMessage,
      searchedMessage,
    ]);
    mocks.api.conversationForTarget.mockResolvedValue(searchedThread);
    mocks.api.content.mockResolvedValue({
      body_text: "Full fictional provider body for attachment choice.",
      body_html:
        "<p>Full fictional <strong>provider body</strong> for attachment choice.</p>",
      attachments: [
        {
          id: "keep-metadata",
          message_id: searchedMessage.id,
          filename: keptAttachment.filename,
          mime_type: keptAttachment.mime_type,
          size_bytes: keptAttachment.size_bytes,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
        {
          id: "remove-metadata",
          message_id: searchedMessage.id,
          filename: removedAttachment.filename,
          mime_type: removedAttachment.mime_type,
          size_bytes: removedAttachment.size_bytes,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
      ],
    });
    mocks.api.forwardAttachments.mockResolvedValue([
      keptAttachment,
      removedAttachment,
    ]);

    const main = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "attachment choice" } },
    );
    expect(await screen.findByText("First account collision")).toBeVisible();
    fireEvent.doubleClick(
      (await screen.findByText(searchedMessage.subject)).closest("button")!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    const readerUrl = mocks.openedUrls[0];
    main.unmount();
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
        accountId: secondAccount.id,
        localMessageId: searchedMessage.id,
        rfcMessageId: searchedMessage.message_id,
        threadId: searchedMessage.thread_id,
        mailbox: searchedMessage.mailbox,
      }),
    );
    fireEvent.click(
      (await screen.findAllByRole("button", { name: "Forward" })).at(-1)!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(2));
    expect(mocks.api.content).toHaveBeenCalledWith(
      searchedMessage.id,
      searchedMessage.mailbox_uid_validity,
    );
    const composeUrl = mocks.openedUrls[1];
    const composeToken = new URL(
      composeUrl,
      window.location.origin,
    ).searchParams.get("seedKey");
    expect(composeToken).toEqual(expect.any(String));
    const storedSeed = JSON.parse(
      localStorage.getItem(`dakia.compose-seed.${composeToken}`) ?? "{}",
    );
    expect(storedSeed).toMatchObject({
      accountId: secondAccount.id,
      subject: `Fwd: ${searchedMessage.subject}`,
      forwardMessageId: searchedMessage.id,
      forwardMessageUidValidity: searchedMessage.mailbox_uid_validity,
      contextMessageIds: [searchedMessage.id],
      contextMessageTargets: [
        {
          messageId: searchedMessage.id,
          expectedUidValidity: searchedMessage.mailbox_uid_validity,
        },
      ],
    });
    expect(storedSeed.body).toContain(
      "Full fictional provider body for attachment choice.",
    );
    expect(storedSeed.bodyHtml).toContain(
      "Full fictional <strong>provider body</strong> for attachment choice.",
    );
    reader.unmount();
    window.history.replaceState({}, "", composeUrl);

    render(<ComposeApp />);
    expect(await screen.findByText(keptAttachment.filename)).toBeVisible();
    expect(screen.getByText(removedAttachment.filename)).toBeVisible();
    expect(screen.getByLabelText("From")).toHaveValue(secondAccount.id);
    expect(mocks.api.forwardAttachments).toHaveBeenCalledWith(
      searchedMessage.id,
      searchedMessage.mailbox_uid_validity,
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: `Remove ${removedAttachment.filename}`,
      }),
    );
    expect(screen.queryByText(removedAttachment.filename)).toBeNull();
    expect(screen.getByText(keptAttachment.filename)).toBeVisible();
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "reviewer@example.test" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: secondAccount.id,
        to: ["reviewer@example.test"],
        cc: [],
        bcc: [],
        subject: `Fwd: ${searchedMessage.subject}`,
        body_text: expect.stringContaining(
          "Full fictional provider body for attachment choice.",
        ),
        body_html: expect.stringContaining(
          "Full fictional <strong>provider body</strong> for attachment choice.",
        ),
        in_reply_to: null,
        references: null,
        attachments: [keptAttachment],
        send_attempt_id: expect.any(String),
      }),
    );
  });

  it("keeps original attachments out of a reply while sending one newly chosen file", async () => {
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0]
        ? [firstAccountCollisionThread, searchedThread]
        : [],
      nextCursor: null,
    }));
    mocks.api.searchRemote.mockResolvedValue([
      firstAccountCollisionMessage,
      searchedMessage,
    ]);
    mocks.api.conversationForTarget.mockResolvedValue(searchedThread);
    mocks.api.content.mockResolvedValue({
      body_text: "Full fictional reply body.",
      body_html: "<p>Full fictional reply body.</p>",
      attachments: [
        {
          id: "original-one",
          message_id: searchedMessage.id,
          filename: "original-one.pdf",
          mime_type: "application/pdf",
          size_bytes: 12,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
        {
          id: "original-two",
          message_id: searchedMessage.id,
          filename: "original-two.txt",
          mime_type: "text/plain",
          size_bytes: 12,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
      ],
    });

    const main = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "reply attachment choice" } },
    );
    expect(await screen.findByText("First account collision")).toBeVisible();
    fireEvent.doubleClick(
      (await screen.findByText(searchedMessage.subject)).closest("button")!,
    );
    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    const readerUrl = mocks.openedUrls[0];
    main.unmount();
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
        accountId: secondAccount.id,
        localMessageId: searchedMessage.id,
        rfcMessageId: searchedMessage.message_id,
        threadId: searchedMessage.thread_id,
        mailbox: searchedMessage.mailbox,
      }),
    );
    fireEvent.click(
      (await screen.findAllByRole("button", { name: "Reply" })).at(-1)!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(2));
    expect(mocks.api.content).toHaveBeenCalledWith(
      searchedMessage.id,
      searchedMessage.mailbox_uid_validity,
    );
    const composeUrl = mocks.openedUrls[1];
    const composeToken = new URL(
      composeUrl,
      window.location.origin,
    ).searchParams.get("seedKey");
    const storedSeed = JSON.parse(
      localStorage.getItem(`dakia.compose-seed.${composeToken}`) ?? "{}",
    );
    expect(storedSeed.accountId).toBe(secondAccount.id);
    expect(storedSeed).not.toHaveProperty("forwardMessageId");
    expect(storedSeed).not.toHaveProperty("attachments");
    expect(storedSeed.contextMessageTargets).toEqual([
      {
        messageId: searchedMessage.id,
        expectedUidValidity: searchedMessage.mailbox_uid_validity,
      },
    ]);
    reader.unmount();
    window.history.replaceState({}, "", composeUrl);

    const compose = render(<ComposeApp />);
    expect(await screen.findByLabelText("From")).toHaveValue(secondAccount.id);
    expect(mocks.api.forwardAttachments).not.toHaveBeenCalled();
    expect(screen.queryByText("original-one.pdf")).toBeNull();
    expect(screen.queryByText("original-two.txt")).toBeNull();

    const fileInput =
      compose.container.querySelector<HTMLInputElement>('input[type="file"]');
    expect(fileInput).not.toBeNull();
    fireEvent.change(fileInput!, {
      target: {
        files: [
          new File(["reply-only"], "reply-only.txt", { type: "text/plain" }),
        ],
      },
    });
    expect(await screen.findByText("reply-only.txt")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: secondAccount.id,
        to: ["Fictional Sender <sender@example.test>"],
        subject: `Re: ${searchedMessage.subject}`,
        attachments: [
          {
            filename: "reply-only.txt",
            mime_type: "text/plain",
            size_bytes: 10,
            content_base64: "cmVwbHktb25seQ==",
          },
        ],
      }),
    );
  });

  it.each(
    [
      { label: "CR", encoded: "%0D", decoded: "\r" },
      { label: "LF", encoded: "%0A", decoded: "\n" },
    ].flatMap((lineBreak) => [
      {
        field: "To",
        lineBreak: lineBreak.label,
        href: `mailto:victim%40example.test${lineBreak.encoded}Bcc%3Aattacker%40example.test?subject=Fictional%20report`,
        decoded: `victim@example.test${lineBreak.decoded}Bcc:attacker@example.test`,
      },
      {
        field: "Cc",
        lineBreak: lineBreak.label,
        href: `mailto:victim%40example.test?cc=copy%40example.test${lineBreak.encoded}Bcc%3Aattacker%40example.test&subject=Fictional%20report`,
        decoded: `copy@example.test${lineBreak.decoded}Bcc:attacker@example.test`,
      },
      {
        field: "Subject",
        lineBreak: lineBreak.label,
        href: `mailto:victim%40example.test?subject=Fictional%20report${lineBreak.encoded}X-Injected%3Ayes`,
        decoded: `Fictional report${lineBreak.decoded}X-Injected:yes`,
      },
    ]),
  )(
    "rejects $lineBreak injection in $field routed through the detached Reader",
    async ({ field, href, decoded }) => {
      const { storedSeed } = await openMailtoThroughDetachedReader(href);

      expect(storedSeed.accountId).toBe(secondAccount.id);
      expect(storedSeed[field.toLowerCase()]).toBe(decoded);
      expect(
        await screen.findByText(
          "Some address or subject fields contain line breaks. Remove them before sending.",
        ),
      ).toHaveAttribute("role", "alert");
      const sendButton = screen.getByRole("button", { name: /^Send/ });
      expect(sendButton).toBeDisabled();
      fireEvent.click(sendButton);
      fireEvent.keyDown(window, { key: "Enter", metaKey: true });

      expect(mocks.api.send).not.toHaveBeenCalled();
      expect(mocks.api.openExternal).not.toHaveBeenCalled();
      expect(mocks.openedUrls).toHaveLength(1);
    },
  );

  it.each([
    {
      field: "To",
      href: "mailto:%22Fictional%250D%250AName%22%20%3Cvictim%40example.test%3E?subject=Fictional%20report",
      decoded: '"Fictional%0D%0AName" <victim@example.test>',
    },
    {
      field: "Cc",
      href: "mailto:victim%40example.test?cc=%22Fictional%250D%250AName%22%20%3Ccopy%40example.test%3E&subject=Fictional%20report",
      decoded: '"Fictional%0D%0AName" <copy@example.test>',
    },
    {
      field: "Subject",
      href: "mailto:victim%40example.test?subject=Fictional%250D%250AX-Not-A-Header%3Ayes",
      decoded: "Fictional%0D%0AX-Not-A-Header:yes",
    },
  ])(
    "decodes a double-encoded $field value only once",
    async ({ field, href, decoded }) => {
      const { storedSeed } = await openMailtoThroughDetachedReader(href);

      expect(storedSeed[field.toLowerCase()]).toBe(decoded);
      expect(storedSeed[field.toLowerCase()]).not.toMatch(/[\r\n]/);
      expect(
        screen.queryByText(
          "Some address or subject fields contain line breaks. Remove them before sending.",
        ),
      ).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

      await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
      const payload = mocks.api.send.mock.calls[0][0];
      expect(
        field === "To"
          ? payload.to[0]
          : field === "Cc"
            ? payload.cc[0]
            : payload.subject,
      ).toBe(decoded);
      expect(mocks.api.openExternal).not.toHaveBeenCalled();
      expect(mocks.openedUrls).toHaveLength(1);
    },
  );

  it("decodes a Reader mailto body once while preserving body line breaks", async () => {
    const href =
      "mailto:reviewer%40example.test" +
      "?subject=Fictional%2520report%250D%250AX-Not-A-Header%3Ayes" +
      "&body=First%20line%0D%0ASecond%20line%20keeps%20%250D%250Atext";
    const { storedSeed } = await openMailtoThroughDetachedReader(href);

    expect(storedSeed).toMatchObject({
      accountId: secondAccount.id,
      to: "reviewer@example.test",
      subject: "Fictional%20report%0D%0AX-Not-A-Header:yes",
      body: "First line\r\nSecond line keeps %0D%0Atext",
    });
    expect(
      screen.queryByText(
        "Some address or subject fields contain line breaks. Remove them before sending.",
      ),
    ).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: secondAccount.id,
        to: ["reviewer@example.test"],
        subject: "Fictional%20report%0D%0AX-Not-A-Header:yes",
        body_text: "First line\nSecond line keeps %0D%0Atext",
      }),
    );
    expect(mocks.api.openExternal).not.toHaveBeenCalled();
    expect(mocks.openedUrls).toHaveLength(1);
  });

  it("keeps Reply All recipients and thread identity through a detached Reader send", async () => {
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0] ? [replyAllThread] : [],
      nextCursor: null,
    }));
    mocks.api.conversationForTarget.mockResolvedValue(replyAllThread);
    mocks.api.content.mockResolvedValue({
      body_text: "Full reply all body",
      body_html: "<p>Full <strong>reply all</strong> body</p>",
      attachments: [],
    });

    const main = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "reply all identity" } },
    );
    fireEvent.doubleClick(
      (await screen.findByText("Reply all identity")).closest("button")!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    const readerUrl = mocks.openedUrls[0];
    main.unmount();
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
        accountId: secondAccount.id,
        localMessageId: replyAllMessage.id,
        rfcMessageId: replyAllMessage.message_id,
        threadId: replyAllMessage.thread_id,
        mailbox: "INBOX",
      }),
    );
    const replyAllButtons = await screen.findAllByRole("button", {
      name: "Reply all",
    });
    fireEvent.click(replyAllButtons.at(-1)!);

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(2));
    expect(mocks.api.content).toHaveBeenCalledWith(
      replyAllMessage.id,
      replyAllMessage.mailbox_uid_validity,
    );
    const composeUrl = mocks.openedUrls[1];
    const composeToken = new URL(
      composeUrl,
      window.location.origin,
    ).searchParams.get("seedKey");
    expect(composeToken).toEqual(expect.any(String));
    const storedSeed = JSON.parse(
      localStorage.getItem(`dakia.compose-seed.${composeToken}`) ?? "{}",
    );
    expect(storedSeed).toMatchObject({
      accountId: secondAccount.id,
      inReplyTo: replyAllMessage.message_id,
      references:
        "<reply-root@example.test> <reply-parent@example.test> <reply-all-message@example.test>",
      contextMessageIds: [replyAllMessage.id],
      contextMessageTargets: [
        {
          messageId: replyAllMessage.id,
          expectedUidValidity: replyAllMessage.mailbox_uid_validity,
        },
      ],
    });
    reader.unmount();
    window.history.replaceState({}, "", composeUrl);

    render(<ComposeApp />);
    expect(await screen.findByLabelText("From")).toHaveValue(secondAccount.id);
    expect(screen.getByLabelText("To")).toHaveValue(
      "Reply Desk <reply@example.test>, Peer Person <peer@example.test>",
    );
    expect(screen.getByLabelText("Cc")).toHaveValue(
      "Other Person <other@example.test>",
    );
    expect(screen.getByLabelText("Bcc")).toHaveValue("");
    expect(screen.getByLabelText("Subject")).toHaveValue(
      "Re: Reply all identity",
    );
    const replyBody = screen.getByRole("textbox", {
      name: "Write your message…",
    });
    await waitFor(() =>
      expect(replyBody).toHaveTextContent("Full reply all body"),
    );
    expect(replyBody.innerHTML).toContain('data-dakia-quoted-email="true"');

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: secondAccount.id,
        to: [
          "Reply Desk <reply@example.test>",
          "Peer Person <peer@example.test>",
        ],
        cc: ["Other Person <other@example.test>"],
        bcc: [],
        subject: "Re: Reply all identity",
        in_reply_to: replyAllMessage.message_id,
        references:
          "<reply-root@example.test> <reply-parent@example.test> <reply-all-message@example.test>",
        body_text: expect.stringContaining("Full reply all body"),
        body_html: expect.stringContaining('data-dakia-quoted-email="true"'),
        send_attempt_id: expect.any(String),
      }),
    );
  });
});
