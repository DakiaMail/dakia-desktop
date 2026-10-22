import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { ReaderWindowSeed } from "./readerWindow";
import type { MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => ({
  api: {
    accounts: vi.fn(),
    action: vi.fn(),
    cancelTranslationModelInstall: vi.fn(),
    content: vi.fn(),
    conversationForTarget: vi.fn(),
    exportMessage: vi.fn(),
    installTranslationModel: vi.fn(),
    saveAllAttachments: vi.fn(),
    saveAttachment: vi.fn(),
    setRead: vi.fn(),
    setStarred: vi.fn(),
    summarize: vi.fn(),
    translationModels: vi.fn(),
    trashMessagesFromSender: vi.fn(),
    unsubscribe: vi.fn(),
  },
  closeReaderWindow: vi.fn(),
  notifyReaderWindowMutated: vi.fn(),
  notifyReaderWindowFailed: vi.fn(),
  notifyReaderWindowReady: vi.fn(),
  onReaderTarget: vi.fn(),
  hasNativeReaderSeedHandoff: vi.fn(),
  rememberReaderSeed: vi.fn(),
  readReaderSeed: vi.fn(),
  openComposeWindow: vi.fn(),
  readerTargetHandler: undefined as
    ((seed: ReaderWindowSeed) => void) | undefined,
  showNativeMessage: vi.fn(),
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./composeWindow", () => ({
  openComposeWindow: mocks.openComposeWindow,
}));
vi.mock("./readerWindow", () => ({
  closeReaderWindow: mocks.closeReaderWindow,
  notifyReaderWindowMutated: mocks.notifyReaderWindowMutated,
  notifyReaderWindowFailed: mocks.notifyReaderWindowFailed,
  notifyReaderWindowReady: mocks.notifyReaderWindowReady,
  onReaderTarget: mocks.onReaderTarget,
  hasNativeReaderSeedHandoff: mocks.hasNativeReaderSeedHandoff,
  rememberReaderSeed: mocks.rememberReaderSeed,
  readReaderSeed: mocks.readReaderSeed,
}));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: vi.fn(),
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./nativeWindows", () => ({
  onNativeMenuAction: vi.fn(async () => () => undefined),
}));
const message: MailSummary = {
  id: "fictional-newsletter-42",
  account_id: "fictional-account",
  mailbox: "INBOX",
  uid: 42,
  mailbox_uid_validity: 901,
  thread_id: "fictional-thread",
  subject: "Fictional newsletter",
  from_address: "list@example.test",
  to_addresses: "reader@example.test",
  received_at: "2026-10-21T09:00:00Z",
  snippet: "A fictional message",
  body_text: "A fictional message",
  is_read: true,
  is_flagged: false,
  has_attachments: false,
  unsubscribe_kind: "one_click",
};

const thread: MailThread = {
  id: "fictional-account:fictional-thread",
  accountId: "fictional-account",
  threadId: "fictional-thread",
  messages: [message],
  latest: message,
  unread: false,
  hasAttachments: false,
  participants: [],
};

const retargetedMessage: MailSummary = {
  ...message,
  id: "fictional-newsletter-43",
  account_id: "fictional-second-account",
  uid: 43,
  mailbox_uid_validity: 902,
  thread_id: "fictional-second-thread",
  subject: "Second fictional newsletter",
  received_at: "2026-10-21T09:01:00Z",
};

const retargetedThread: MailThread = {
  ...thread,
  id: "fictional-second-account:fictional-second-thread",
  accountId: retargetedMessage.account_id,
  threadId: retargetedMessage.thread_id,
  messages: [retargetedMessage],
  latest: retargetedMessage,
};

async function retargetReaderToSecondMessage() {
  await waitFor(() => expect(mocks.readerTargetHandler).toBeTypeOf("function"));
  await act(async () =>
    mocks.readerTargetHandler!({
      target: {
        accountId: retargetedMessage.account_id,
        threadId: retargetedMessage.thread_id,
        localMessageId: retargetedMessage.id,
      },
      focusedMessageId: retargetedMessage.id,
    }),
  );
  await screen.findByText(retargetedMessage.subject);
}

async function retargetReaderToOriginalMessage() {
  await waitFor(() => expect(mocks.readerTargetHandler).toBeTypeOf("function"));
  await act(async () =>
    mocks.readerTargetHandler!({
      target: {
        accountId: message.account_id,
        threadId: message.thread_id,
        localMessageId: message.id,
      },
      focusedMessageId: message.id,
    }),
  );
  await screen.findByText(message.subject);
}

describe("ReaderWindowApp unsubscribe integration", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.readerTargetHandler = undefined;
    mocks.hasNativeReaderSeedHandoff.mockReturnValue(false);
    mocks.notifyReaderWindowReady.mockResolvedValue(undefined);
    mocks.readReaderSeed.mockReturnValue({
      target: {
        accountId: message.account_id,
        threadId: message.thread_id,
        localMessageId: message.id,
      },
      focusedMessageId: message.id,
    });
    mocks.api.accounts.mockResolvedValue([
      { id: message.account_id, email: "reader@example.test" },
      { id: retargetedMessage.account_id, email: "other@example.test" },
    ]);
    mocks.api.conversationForTarget.mockImplementation(async (target) =>
      target.accountId === retargetedMessage.account_id
        ? retargetedThread
        : thread,
    );
    mocks.api.content.mockResolvedValue({
      body_text: message.body_text,
      unsubscribe_kind: "one_click",
      attachments: [],
    });
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.api.unsubscribe.mockResolvedValue({
      kind: "completed",
      cleanupTarget: null,
    });
    mocks.onReaderTarget.mockImplementation(async (handler) => {
      mocks.readerTargetHandler = handler;
      return () => undefined;
    });
    mocks.notifyReaderWindowMutated.mockResolvedValue(undefined);
    mocks.showNativeMessage.mockResolvedValue(undefined);
  });

  it("does not render an action or contact the unsubscribe command when signed one-click metadata is absent", async () => {
    const messageWithoutAction = {
      ...message,
      unsubscribe_kind: undefined,
    };
    mocks.api.conversationForTarget.mockResolvedValue({
      ...thread,
      messages: [messageWithoutAction],
      latest: messageWithoutAction,
    });
    mocks.api.content.mockResolvedValue({
      body_text: message.body_text,
      attachments: [],
    });

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    await screen.findByText(message.subject);
    expect(
      screen.queryByRole("button", { name: "Unsubscribe" }),
    ).not.toBeInTheDocument();
    expect(mocks.api.unsubscribe).not.toHaveBeenCalled();
  });

  it("keeps an in-flight unsubscribe disabled when the same immutable message is refocused", async () => {
    let rejectFirst: (error: Error) => void = () => undefined;
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((_, reject: (error: Error) => void) => {
          rejectFirst = reject;
        }),
    );

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    const unsubscribe = await screen.findByRole("button", {
      name: "Unsubscribe",
    });
    fireEvent.click(unsubscribe);
    expect(mocks.api.unsubscribe).toHaveBeenCalledWith(message.id, 901);
    await waitFor(() =>
      expect(mocks.readerTargetHandler).toBeTypeOf("function"),
    );
    await act(async () =>
      mocks.readerTargetHandler!({
        target: {
          accountId: message.account_id,
          threadId: message.thread_id,
          localMessageId: message.id,
        },
        focusedMessageId: message.id,
      }),
    );
    await screen.findByText(message.subject);

    const refocused = screen.getByRole("button", { name: "Unsubscribe" });
    expect(refocused).toBeDisabled();
    fireEvent.click(refocused);
    expect(mocks.api.unsubscribe).toHaveBeenCalledOnce();

    await act(async () =>
      rejectFirst(new Error("fictional refocused provider rejection")),
    );
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "fictional refocused provider rejection",
        "error",
      ),
    );
    await waitFor(() => expect(refocused).toBeEnabled());
  });

  it("keeps each immutable target pending across A to B to A focus changes", async () => {
    let resolveFirst: (value: {
      kind: "completed";
      cleanupTarget: null;
    }) => void = () => undefined;
    let resolveSecond: (value: {
      kind: "completed";
      cleanupTarget: null;
    }) => void = () => undefined;
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveFirst = resolve;
        }),
    );
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveSecond = resolve;
        }),
    );

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    await retargetReaderToSecondMessage();
    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    await waitFor(() => expect(mocks.api.unsubscribe).toHaveBeenCalledTimes(2));

    await retargetReaderToOriginalMessage();
    const unsubscribeForOriginalMessage = screen.getByRole("button", {
      name: "Unsubscribe",
    });
    expect(unsubscribeForOriginalMessage).toBeDisabled();
    fireEvent.click(unsubscribeForOriginalMessage);
    expect(mocks.api.unsubscribe).toHaveBeenCalledTimes(2);

    await act(async () =>
      resolveSecond({ kind: "completed", cleanupTarget: null }),
    );
    expect(unsubscribeForOriginalMessage).toBeDisabled();

    await act(async () =>
      resolveFirst({ kind: "completed", cleanupTarget: null }),
    );
    await waitFor(() => expect(unsubscribeForOriginalMessage).toBeEnabled());
    expect(await screen.findByText("Unsubscribe request sent.")).toBeVisible();
  });

  it("waits for an explicit Reader action, reports a provider error, and retries only the newly displayed immutable target", async () => {
    let rejectFirst: (error: Error) => void = () => undefined;
    let rejectSecond: (error: Error) => void = () => undefined;
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((_, reject: (error: Error) => void) => {
          rejectFirst = reject;
        }),
    );
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((_, reject: (error: Error) => void) => {
          rejectSecond = reject;
        }),
    );

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    const unsubscribe = await screen.findByRole("button", {
      name: "Unsubscribe",
    });
    expect(mocks.api.unsubscribe).not.toHaveBeenCalled();

    fireEvent.click(unsubscribe);
    expect(mocks.api.unsubscribe).toHaveBeenCalledWith(message.id, 901);
    expect(unsubscribe).toBeDisabled();
    fireEvent.click(unsubscribe);
    expect(mocks.api.unsubscribe).toHaveBeenCalledOnce();

    await retargetReaderToSecondMessage();

    const unsubscribeForSecondMessage = screen.getByRole("button", {
      name: "Unsubscribe",
    });
    await waitFor(() => expect(unsubscribeForSecondMessage).toBeEnabled());
    fireEvent.click(unsubscribeForSecondMessage);
    await waitFor(() =>
      expect(mocks.api.unsubscribe).toHaveBeenLastCalledWith(
        retargetedMessage.id,
        902,
      ),
    );
    expect(mocks.api.unsubscribe).toHaveBeenCalledTimes(2);

    await act(async () =>
      rejectFirst(new Error("fictional provider rejected")),
    );
    expect(mocks.showNativeMessage).not.toHaveBeenCalled();
    expect(screen.queryByText("Unsubscribe request sent.")).toBeNull();
    const retryAfterStaleFailure = screen.getByRole("button", {
      name: "Unsubscribe",
    });
    expect(retryAfterStaleFailure).toBeDisabled();
    await act(async () =>
      rejectSecond(new Error("fictional second provider rejected")),
    );
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "fictional second provider rejected",
        "error",
      ),
    );
    const retry = screen.getByRole("button", { name: "Unsubscribe" });
    await waitFor(() => expect(retry).toBeEnabled());

    fireEvent.click(retry);
    await waitFor(() =>
      expect(mocks.api.unsubscribe).toHaveBeenLastCalledWith(
        retargetedMessage.id,
        902,
      ),
    );
    expect(mocks.api.unsubscribe).toHaveBeenCalledTimes(3);
    expect(await screen.findByText("Unsubscribe request sent.")).toBeVisible();
  });

  it("does not expose a stale successful unsubscribe or its sender-cleanup action after retargeting", async () => {
    let resolveFirst: (value: {
      kind: "completed";
      cleanupTarget: {
        accountId: string;
        senderName: string;
        senderAddress: string;
      };
    }) => void = () => undefined;
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveFirst = resolve;
        }),
    );

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    await retargetReaderToSecondMessage();
    await act(async () =>
      resolveFirst({
        kind: "completed",
        cleanupTarget: {
          accountId: message.account_id,
          senderName: "Fictional list",
          senderAddress: message.from_address,
        },
      }),
    );

    await waitFor(() => expect(mocks.api.unsubscribe).toHaveBeenCalledOnce());
    expect(screen.queryByText("Unsubscribe request sent.")).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Move to Trash" }),
    ).not.toBeInTheDocument();
    expect(mocks.api.trashMessagesFromSender).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    await waitFor(() =>
      expect(mocks.api.unsubscribe).toHaveBeenLastCalledWith(
        retargetedMessage.id,
        902,
      ),
    );
    expect(await screen.findByText("Unsubscribe request sent.")).toBeVisible();
  });

  it("does not expose an old target outcome while a newly accepted target is still loading", async () => {
    let resolveFirst: (value: {
      kind: "completed";
      cleanupTarget: {
        accountId: string;
        senderName: string;
        senderAddress: string;
      };
    }) => void = () => undefined;
    let resolveSecondThread: ((value: MailThread) => void) | undefined;
    mocks.api.unsubscribe.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveFirst = resolve;
        }),
    );
    mocks.api.conversationForTarget.mockImplementation((target) =>
      target.accountId === retargetedMessage.account_id
        ? new Promise<MailThread>((resolve) => {
            resolveSecondThread = resolve;
          })
        : Promise.resolve(thread),
    );

    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    await waitFor(() =>
      expect(mocks.readerTargetHandler).toBeTypeOf("function"),
    );
    await act(async () => {
      mocks.readerTargetHandler!({
        target: {
          accountId: retargetedMessage.account_id,
          threadId: retargetedMessage.thread_id,
          localMessageId: retargetedMessage.id,
        },
        focusedMessageId: retargetedMessage.id,
      });
      resolveFirst({
        kind: "completed",
        cleanupTarget: {
          accountId: message.account_id,
          senderName: "Fictional list",
          senderAddress: message.from_address,
        },
      });
    });

    expect(screen.queryByText("Unsubscribe request sent.")).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Move to Trash" }),
    ).not.toBeInTheDocument();
    expect(mocks.api.trashMessagesFromSender).not.toHaveBeenCalled();

    await waitFor(() => expect(resolveSecondThread).toBeTypeOf("function"));
    await act(async () => resolveSecondThread!(retargetedThread));
    await screen.findByText(retargetedMessage.subject);
  });
});
