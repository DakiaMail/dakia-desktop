import { MantineProvider } from "@mantine/core";
import { act, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { ReaderWindowApp } from "./ReaderWindowApp";
import { readerWindowLabel } from "./readerWindow";
import type { Account, MailSummary, MailThread } from "./types";

const eventMocks = vi.hoisted(() => ({
  emitTo: vi.fn(),
  listener: undefined as ((event: { payload: unknown }) => void) | undefined,
  listen: vi.fn(),
}));
const apiMocks = vi.hoisted(() => ({
  accounts: vi.fn(),
  aiAvailable: vi.fn(),
  conversationForTarget: vi.fn(),
  setRead: vi.fn(),
}));
const feedbackMocks = vi.hoisted(() => ({ showNativeMessage: vi.fn() }));
const close = vi.fn();
let currentLabel = "reader-unset";

vi.mock("@tauri-apps/api/event", () => eventMocks);
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: currentLabel, close }),
}));
vi.mock("./api", () => ({ api: apiMocks }));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./nativeFeedback", () => feedbackMocks);
vi.mock("./nativeWindows", () => ({
  onNativeMenuAction: vi.fn(async () => () => undefined),
}));
vi.mock("./composeWindow", () => ({ openComposeWindow: vi.fn() }));
vi.mock("./components/Reader", () => ({
  Reader: ({ message }: { message?: MailSummary }) => (
    <span data-testid="reader-message">{message?.id}</span>
  ),
}));

const account: Account = {
  id: "account-1",
  email: "reader@example.test",
  account_name: "Reader",
  display_name: "Reader",
  provider_id: "fastmail",
  auth: { type: "password", username: "reader@example.test" },
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

function thread(id: string, threadId: string): MailThread {
  const message: MailSummary = {
    id,
    account_id: account.id,
    mailbox: "INBOX",
    uid: id === "message-a" ? 1 : 2,
    mailbox_uid_validity: 77,
    thread_id: threadId,
    subject: id,
    from_address: "sender@example.test",
    to_addresses: account.email,
    received_at: "2026-10-21T10:00:00Z",
    snippet: id,
    body_text: id,
    is_read: true,
    is_flagged: false,
    has_attachments: false,
  };
  return {
    id: `${account.id}:${threadId}`,
    accountId: account.id,
    threadId,
    messages: [message],
    latest: message,
    unread: false,
    hasAttachments: false,
    participants: [],
  };
}

describe("ReaderWindowApp native seed handoff", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    sessionStorage.clear();
    close.mockResolvedValue(undefined);
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    eventMocks.listen.mockImplementation(async (event, handler) => {
      if (event === "reader-target") eventMocks.listener = handler;
      return () => {
        if (event === "reader-target") eventMocks.listener = undefined;
      };
    });
    apiMocks.accounts.mockResolvedValue([account]);
    apiMocks.aiAvailable.mockResolvedValue(false);
    apiMocks.setRead.mockResolvedValue(undefined);
    apiMocks.conversationForTarget.mockImplementation(async (target) =>
      target.localMessageId === "message-b"
        ? thread("message-b", "thread-a")
        : thread("message-a", "thread-a"),
    );
  });

  it("delivers A once in StrictMode, retargets B, and reloads B without opener storage", async () => {
    const token = "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6";
    const a = {
      target: { accountId: account.id, threadId: "thread-a" },
      focusedMessageId: "message-a",
    };
    const b = {
      target: {
        accountId: account.id,
        threadId: "thread-a",
        localMessageId: "message-b",
      },
      focusedMessageId: "message-b",
    };
    currentLabel = await readerWindowLabel(a.target);
    window.history.replaceState({}, "", `/?view=reader&seedDbKey=${token}`);

    const first = render(
      <MantineProvider>
        <StrictMode>
          <ReaderWindowApp />
        </StrictMode>
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(eventMocks.listener).toEqual(expect.any(Function)),
    );
    expect(eventMocks.emitTo).toHaveBeenCalledWith("main", "reader-ready", {
      label: currentLabel,
      seedToken: token,
    });
    await act(async () =>
      eventMocks.listener?.({
        payload: { label: currentLabel, seedToken: token, seed: a },
      }),
    );
    expect(await screen.findByTestId("reader-message")).toHaveTextContent(
      "message-a",
    );
    await act(async () =>
      eventMocks.listener?.({ payload: { label: currentLabel, seed: b } }),
    );
    await waitFor(() =>
      expect(screen.getByTestId("reader-message")).toHaveTextContent(
        "message-b",
      ),
    );
    expect(sessionStorage.getItem("dakia.reader-active-seed")).toContain(
      "message-b",
    );
    first.unmount();

    render(
      <MantineProvider>
        <StrictMode>
          <ReaderWindowApp />
        </StrictMode>
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(screen.getByTestId("reader-message")).toHaveTextContent(
        "message-b",
      ),
    );
    expect(close).not.toHaveBeenCalled();
    expect(feedbackMocks.showNativeMessage).not.toHaveBeenCalled();
  });
});
