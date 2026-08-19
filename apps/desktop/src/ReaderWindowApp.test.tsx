import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => ({
  api: {
    accounts: vi.fn(),
    action: vi.fn(),
    aiAvailable: vi.fn(),
    content: vi.fn(),
    conversationForTarget: vi.fn(),
    setRead: vi.fn(),
    setStarred: vi.fn(),
    summarize: vi.fn(),
    unsubscribe: vi.fn(),
  },
  closeReaderWindow: vi.fn(),
  notifyReaderWindowMutated: vi.fn(),
  notifyReaderWindowFailed: vi.fn(),
  onReaderTarget: vi.fn(),
  readReaderSeed: vi.fn(),
  openComposeWindow: vi.fn(),
  showNativeMessage: vi.fn(),
  nativeMenuHandlers: [] as Array<(action: string) => void>,
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./composeWindow", () => ({
  openComposeWindow: mocks.openComposeWindow,
}));
vi.mock("./readerWindow", () => ({
  closeReaderWindow: mocks.closeReaderWindow,
  notifyReaderWindowMutated: mocks.notifyReaderWindowMutated,
  notifyReaderWindowFailed: mocks.notifyReaderWindowFailed,
  onReaderTarget: mocks.onReaderTarget,
  readReaderSeed: mocks.readReaderSeed,
}));
vi.mock("./nativeFeedback", () => ({
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./nativeWindows", () => ({
  onNativeMenuAction: vi.fn(async (handler: (action: string) => void) => {
    mocks.nativeMenuHandlers.push(handler);
    return () => undefined;
  }),
}));
vi.mock("./components/Reader", () => ({
  Reader: ({
    message,
    messages,
    onArchive,
    onPermanentDelete,
  }: {
    message?: MailSummary;
    messages?: MailSummary[];
    onArchive: () => void;
    onPermanentDelete: (message: MailSummary) => void;
  }) => (
    <div>
      <span data-testid="focused-message">{message?.id}</span>
      <span data-testid="conversation-count">{messages?.length}</span>
      <button type="button" onClick={onArchive}>
        Archive
      </button>
      <button
        type="button"
        onClick={() => message && onPermanentDelete(message)}
      >
        Permanently delete
      </button>
    </div>
  ),
}));

const messages: MailSummary[] = [
  {
    id: "message-1",
    account_id: "account-1",
    mailbox: "INBOX",
    uid: 1,
    thread_id: "thread-1",
    subject: "Project",
    from_address: "mara@example.com",
    to_addresses: "alex@example.com",
    received_at: "2026-08-10T09:00:00Z",
    snippet: "Earlier",
    body_text: "Earlier",
    is_read: true,
    is_flagged: false,
    has_attachments: false,
  },
  {
    id: "message-2",
    account_id: "account-1",
    mailbox: "INBOX",
    uid: 2,
    thread_id: "thread-1",
    subject: "Project",
    from_address: "alex@example.com",
    to_addresses: "mara@example.com",
    received_at: "2026-08-10T10:00:00Z",
    snippet: "Focused",
    body_text: "Focused",
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  },
];

const thread: MailThread = {
  id: "account-1:thread-1",
  accountId: "account-1",
  threadId: "thread-1",
  messages,
  latest: messages[1],
  unread: true,
  hasAttachments: false,
  participants: [],
};

describe("ReaderWindowApp", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.nativeMenuHandlers.length = 0;
    mocks.readReaderSeed.mockReturnValue({
      target: {
        accountId: "account-1",
        threadId: "thread-1",
        localMessageId: "message-2",
      },
      focusedMessageId: "message-1",
    });
    mocks.api.accounts.mockResolvedValue([
      { id: "account-1", email: "alex@example.com" },
    ]);
    mocks.api.conversationForTarget.mockResolvedValue(thread);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.action.mockResolvedValue(undefined);
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.onReaderTarget.mockResolvedValue(() => undefined);
    mocks.notifyReaderWindowMutated.mockResolvedValue(undefined);
  });

  it("loads the whole target conversation while passing the focused message to Reader", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    expect(await screen.findByTestId("focused-message")).toHaveTextContent(
      "message-1",
    );
    expect(screen.getByTestId("conversation-count")).toHaveTextContent("2");
    expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      localMessageId: "message-2",
    });
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-2", true),
    );
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-2"],
      mutation: "read",
    });
  });

  it("only closes after every mailbox action succeeds and refreshes main", async () => {
    const remainingThread = {
      ...thread,
      messages: [messages[0]],
      latest: messages[0],
    };
    mocks.api.conversationForTarget
      .mockResolvedValueOnce(thread)
      .mockResolvedValueOnce(remainingThread);
    mocks.api.action.mockRejectedValueOnce(new Error("offline"));
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    const archive = await screen.findByRole("button", { name: "Archive" });
    await waitFor(() => expect(mocks.api.setRead).toHaveBeenCalled());
    mocks.notifyReaderWindowMutated.mockClear();

    fireEvent.click(archive);
    await waitFor(() => expect(mocks.api.action).toHaveBeenCalledTimes(2));
    expect(mocks.closeReaderWindow).not.toHaveBeenCalled();
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-2"],
      mutation: "archive",
    });
    await waitFor(() =>
      expect(screen.getByTestId("conversation-count")).toHaveTextContent("1"),
    );

    mocks.api.action.mockResolvedValue(undefined);
    mocks.notifyReaderWindowMutated.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    await waitFor(() => expect(mocks.closeReaderWindow).toHaveBeenCalledOnce());
    expect(mocks.api.action).toHaveBeenCalledTimes(3);
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-1"],
      mutation: "archive",
    });
  });

  it("routes a native archive command to the visible reader conversation", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");
    await waitFor(() =>
      expect(mocks.nativeMenuHandlers.length).toBeGreaterThan(1),
    );

    act(() => mocks.nativeMenuHandlers.at(-1)?.("archive"));

    await waitFor(() => expect(mocks.api.action).toHaveBeenCalledTimes(2));
    expect(mocks.closeReaderWindow).toHaveBeenCalledOnce();
  });

  it("removes only the permanently deleted message and keeps the conversation open", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");

    fireEvent.click(screen.getByRole("button", { name: "Permanently delete" }));

    await waitFor(() =>
      expect(screen.getByTestId("conversation-count")).toHaveTextContent("1"),
    );
    expect(mocks.api.action).toHaveBeenCalledWith(
      "account-1",
      "INBOX",
      1,
      "delete",
    );
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-1"],
      mutation: "delete",
    });
    expect(mocks.closeReaderWindow).not.toHaveBeenCalled();
  });

  it("returns an unresolved reader target to the main Inbox", async () => {
    mocks.api.conversationForTarget.mockResolvedValueOnce(null);
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.notifyReaderWindowFailed).toHaveBeenCalledWith({
        accountId: "account-1",
      }),
    );
    expect(mocks.showNativeMessage).toHaveBeenCalled();
    expect(mocks.closeReaderWindow).toHaveBeenCalled();
  });

  it("does not let a slower prior target overwrite a retargeted window", async () => {
    let resolveFirst: (value: MailThread) => void = () => undefined;
    const first = new Promise<MailThread>((resolve) => {
      resolveFirst = resolve;
    });
    const retargetedMessage = {
      ...messages[1],
      id: "message-retargeted",
      thread_id: "thread-2",
      subject: "Retargeted",
    };
    const retargetedThread = {
      ...thread,
      id: "account-1:thread-2",
      threadId: "thread-2",
      messages: [retargetedMessage],
      latest: retargetedMessage,
    };
    mocks.api.conversationForTarget
      .mockImplementationOnce(() => first)
      .mockResolvedValueOnce(retargetedThread);
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.onReaderTarget).toHaveBeenCalled());
    const retarget = mocks.onReaderTarget.mock.calls[0][0];

    await act(async () => {
      retarget({
        target: { accountId: "account-1", threadId: "thread-2" },
        focusedMessageId: retargetedMessage.id,
      });
    });
    expect(await screen.findByTestId("focused-message")).toHaveTextContent(
      retargetedMessage.id,
    );

    await act(async () => resolveFirst(thread));
    expect(screen.getByTestId("focused-message")).toHaveTextContent(
      retargetedMessage.id,
    );
  });
});
