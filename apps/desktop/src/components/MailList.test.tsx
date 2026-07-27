import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "../i18n";
import type { PendingMailActions } from "../mailActions";
import type { MailSummary } from "../types";
import { groupMessages } from "../threads";
import { MailList } from "./MailList";

const messages: MailSummary[] = ["1", "2"].map((id) => ({
  id,
  account_id: "account",
  mailbox: "INBOX",
  uid: Number(id),
  thread_id: id,
  subject: `Subject ${id}`,
  from_address: `sender${id}@example.com`,
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
}));

function renderList(
  pendingActions: PendingMailActions,
  disabled = false,
  mailboxTitle = "Inbox",
  overrides: {
    messages?: MailSummary[];
    onReplyThread?: (thread: ReturnType<typeof groupMessages>[number]) => void;
    onForwardThread?: (
      thread: ReturnType<typeof groupMessages>[number],
    ) => void;
    onActionThread?: (
      thread: ReturnType<typeof groupMessages>[number],
      action: "archive" | "spam" | "not_spam" | "trash",
    ) => void;
    onToggleReadThread?: (
      thread: ReturnType<typeof groupMessages>[number],
      read: boolean,
    ) => void;
    onToggleStarThread?: (
      thread: ReturnType<typeof groupMessages>[number],
      flagged: boolean,
    ) => void;
    onCategorize?: Parameters<typeof MailList>[0]["onCategorize"];
    aiConnected?: boolean;
  } = {},
) {
  const handlers = {
    onReplyThread: overrides.onReplyThread ?? vi.fn(),
    onForwardThread: overrides.onForwardThread ?? vi.fn(),
    onActionThread: overrides.onActionThread ?? vi.fn(),
    onToggleReadThread: overrides.onToggleReadThread ?? vi.fn(),
    onToggleStarThread: overrides.onToggleStarThread ?? vi.fn(),
    onCategorize: overrides.onCategorize ?? vi.fn(),
  };
  render(
    <MantineProvider>
      <MailList
        threads={groupMessages(overrides.messages ?? messages)}
        selected={new Set(["1"])}
        query=""
        loading={false}
        loadingMore={false}
        hasMore={false}
        remoteSearchUnavailable={false}
        classifying={false}
        aiConnected={overrides.aiConnected ?? false}
        mailboxTitle={mailboxTitle}
        view="list"
        smartInbox={false}
        onViewChange={vi.fn()}
        onCategorize={handlers.onCategorize}
        onToggleStar={vi.fn()}
        onReplyThread={handlers.onReplyThread}
        onForwardThread={handlers.onForwardThread}
        onActionThread={handlers.onActionThread}
        onToggleReadThread={handlers.onToggleReadThread}
        onToggleStarThread={handlers.onToggleStarThread}
        onQuery={vi.fn()}
        onOpen={vi.fn()}
        onSelect={vi.fn()}
        onSync={vi.fn()}
        onCompose={vi.fn()}
        onArchive={vi.fn()}
        onSpam={vi.fn()}
        onSummarize={vi.fn()}
        onLoadMore={vi.fn()}
        pendingActions={pendingActions}
        actionsDisabled={disabled}
        searchRef={{ current: null }}
      />
    </MantineProvider>,
  );
}

describe("MailList action feedback", () => {
  it("keeps batch AI actions hidden even when a provider is connected", () => {
    renderList({}, false, "Inbox", { aiConnected: true });

    expect(screen.queryByLabelText("Summarize")).toBeNull();
  });

  it("marks an optimistic row as exiting without showing a mailbox total", () => {
    renderList({ 1: { action: "archive", phase: "exiting", delay: 24 } }, true);

    expect(screen.queryByText(/conversation$/)).not.toBeInTheDocument();
    const row = screen.getByText("Subject 1").closest("button");
    expect(row).toHaveAttribute("data-action-phase", "exiting");
    expect(row).toBeDisabled();
    expect(screen.getByLabelText("Archive")).toBeDisabled();
  });

  it("loads the next page before the user reaches the end", () => {
    const onLoadMore = vi.fn();
    render(
      <MantineProvider>
        <MailList
          threads={groupMessages(messages)}
          selected={new Set()}
          query=""
          loading={false}
          loadingMore={false}
          hasMore
          remoteSearchUnavailable={false}
          classifying={false}
          aiConnected={false}
          mailboxTitle="Inbox"
          view="list"
          smartInbox={false}
          onViewChange={vi.fn()}
          onCategorize={vi.fn()}
          onToggleStar={vi.fn()}
          onReplyThread={vi.fn()}
          onForwardThread={vi.fn()}
          onActionThread={vi.fn()}
          onToggleReadThread={vi.fn()}
          onToggleStarThread={vi.fn()}
          onQuery={vi.fn()}
          onOpen={vi.fn()}
          onSelect={vi.fn()}
          onSync={vi.fn()}
          onCompose={vi.fn()}
          onArchive={vi.fn()}
          onSpam={vi.fn()}
          onSummarize={vi.fn()}
          onLoadMore={onLoadMore}
          pendingActions={{}}
          actionsDisabled={false}
          searchRef={{ current: null }}
        />
      </MantineProvider>,
    );

    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    expect(onLoadMore).toHaveBeenCalledOnce();
  });

  it("renders a failed row in the restoring phase", () => {
    renderList({ 1: { action: "spam", phase: "restoring", delay: 0 } });
    expect(screen.getByText("Subject 1").closest("button")).toHaveAttribute(
      "data-action-phase",
      "restoring",
    );
  });

  it("shows readable text instead of raw HTML in an email snippet", () => {
    renderList({}, false, "Inbox", {
      messages: [
        {
          ...messages[0],
          snippet:
            "<style>.preview { color: red }</style><p>Your order&nbsp;is <strong>ready</strong>.</p>",
        },
      ],
    });

    expect(screen.getByText("Your order is ready.")).toBeVisible();
    expect(screen.queryByText(/<strong>/)).not.toBeInTheDocument();
    expect(screen.queryByText(/color: red/)).not.toBeInTheDocument();
  });

  it("keeps a long mailbox title in a truncatable heading beside Compose", () => {
    const longTitle = "a.very.long.mailbox.address@example.test";
    renderList({}, false, longTitle);

    const title = screen.getByTitle(longTitle);
    expect(title).toHaveClass("list-title");
    expect(title.parentElement).toHaveClass("list-title-copy");
    expect(screen.getByRole("button", { name: "Compose" })).toBeVisible();
  });

  it("shows a grouped count and selects the conversation once", () => {
    const onSelect = vi.fn();
    render(
      <MantineProvider>
        <MailList
          threads={groupMessages([
            messages[0],
            { ...messages[1], thread_id: messages[0].thread_id },
          ])}
          selected={new Set()}
          query=""
          loading={false}
          loadingMore={false}
          hasMore={false}
          remoteSearchUnavailable={false}
          classifying={false}
          aiConnected={false}
          mailboxTitle="Inbox"
          view="list"
          smartInbox={false}
          onViewChange={vi.fn()}
          onCategorize={vi.fn()}
          onToggleStar={vi.fn()}
          onReplyThread={vi.fn()}
          onForwardThread={vi.fn()}
          onActionThread={vi.fn()}
          onToggleReadThread={vi.fn()}
          onToggleStarThread={vi.fn()}
          onQuery={vi.fn()}
          onOpen={vi.fn()}
          onSelect={onSelect}
          onSync={vi.fn()}
          onCompose={vi.fn()}
          onArchive={vi.fn()}
          onSpam={vi.fn()}
          onSummarize={vi.fn()}
          onLoadMore={vi.fn()}
          pendingActions={{}}
          actionsDisabled={false}
          searchRef={{ current: null }}
        />
      </MantineProvider>,
    );

    expect(
      screen.getByLabelText("2 messages in this conversation"),
    ).toBeVisible();
    screen.getByLabelText("Select").click();
    expect(onSelect).toHaveBeenCalledWith(["account:1"], true);
  });

  it("partitions starred, unread categories, and seen without duplication", () => {
    const smartMessages: MailSummary[] = [
      ...["1", "2", "3", "4"].map((id) => ({
        ...messages[0],
        id: `people-${id}`,
        uid: Number(id),
        thread_id: `people-${id}`,
        subject: `People ${id}`,
        category: "people" as const,
      })),
      {
        ...messages[0],
        id: "seen",
        uid: 10,
        thread_id: "seen",
        subject: "Seen message",
        is_read: true,
        category: "transactions",
      },
      {
        ...messages[0],
        id: "starred",
        uid: 11,
        thread_id: "starred",
        subject: "Starred message",
        is_read: true,
        is_flagged: true,
        category: "newsletters",
      },
    ];
    render(
      <MantineProvider>
        <MailList
          threads={groupMessages(smartMessages)}
          selected={new Set()}
          query=""
          loading={false}
          loadingMore={false}
          hasMore={false}
          remoteSearchUnavailable={false}
          classifying={false}
          aiConnected={false}
          mailboxTitle="Inbox"
          view="smart"
          smartInbox
          onViewChange={vi.fn()}
          onCategorize={vi.fn()}
          onToggleStar={vi.fn()}
          onReplyThread={vi.fn()}
          onForwardThread={vi.fn()}
          onActionThread={vi.fn()}
          onToggleReadThread={vi.fn()}
          onToggleStarThread={vi.fn()}
          onQuery={vi.fn()}
          onOpen={vi.fn()}
          onSelect={vi.fn()}
          onSync={vi.fn()}
          onCompose={vi.fn()}
          onArchive={vi.fn()}
          onSpam={vi.fn()}
          onSummarize={vi.fn()}
          onLoadMore={vi.fn()}
          pendingActions={{}}
          actionsDisabled={false}
          searchRef={{ current: null }}
        />
      </MantineProvider>,
    );

    expect(screen.getByRole("region", { name: "Starred" })).toHaveTextContent(
      "Starred message",
    );
    expect(screen.getByRole("region", { name: "Seen" })).toHaveTextContent(
      "Seen message",
    );
    const people = screen.getByRole("region", { name: "People" });
    expect(people.querySelectorAll(".mail-item")).toHaveLength(4);
    expect(
      screen.queryByRole("button", { name: "More actions" }),
    ).not.toBeInTheDocument();
    expect(screen.getAllByText("Starred message")).toHaveLength(1);
  });

  it("keeps the active thread in its smart category after opening marks it read", () => {
    const activeMessage: MailSummary = {
      ...messages[0],
      id: "active",
      thread_id: "active",
      subject: "Active message",
      is_read: true,
      category: "people",
    };
    render(
      <MantineProvider>
        <MailList
          threads={groupMessages([
            activeMessage,
            {
              ...messages[1],
              id: "seen",
              thread_id: "seen",
              subject: "Seen message",
              is_read: true,
              category: "people",
            },
          ])}
          activeThreadId="account:active"
          selected={new Set()}
          query=""
          loading={false}
          loadingMore={false}
          hasMore={false}
          remoteSearchUnavailable={false}
          classifying={false}
          aiConnected={false}
          mailboxTitle="Inbox"
          view="smart"
          smartInbox
          onViewChange={vi.fn()}
          onCategorize={vi.fn()}
          onToggleStar={vi.fn()}
          onReplyThread={vi.fn()}
          onForwardThread={vi.fn()}
          onActionThread={vi.fn()}
          onToggleReadThread={vi.fn()}
          onToggleStarThread={vi.fn()}
          onQuery={vi.fn()}
          onOpen={vi.fn()}
          onSelect={vi.fn()}
          onSync={vi.fn()}
          onCompose={vi.fn()}
          onArchive={vi.fn()}
          onSpam={vi.fn()}
          onSummarize={vi.fn()}
          onLoadMore={vi.fn()}
          pendingActions={{}}
          actionsDisabled={false}
          searchRef={{ current: null }}
        />
      </MantineProvider>,
    );

    expect(screen.getByRole("region", { name: "People" })).toHaveTextContent(
      "Active message",
    );
    expect(screen.getByRole("region", { name: "Seen" })).toHaveTextContent(
      "Seen message",
    );
    expect(screen.getByRole("region", { name: "Seen" })).not.toHaveTextContent(
      "Active message",
    );
  });

  it("opens row actions for the clicked conversation, not the selection", async () => {
    const onActionThread = vi.fn();
    const onReplyThread = vi.fn();
    renderList({}, false, "Inbox", { onActionThread, onReplyThread });

    fireEvent.contextMenu(screen.getByText("Subject 2").closest("button")!, {
      clientX: 140,
      clientY: 90,
    });

    expect(
      await screen.findByRole("menuitem", { name: "Reply", hidden: true }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: "Forward", hidden: true }),
    ).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("menuitem", { name: "Mark as spam", hidden: true }),
    );

    expect(onActionThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:2" }),
      "spam",
    );
    expect(onReplyThread).not.toHaveBeenCalled();
  });

  it("runs reply and forward from the thread context menu", async () => {
    const onReplyThread = vi.fn();
    const onForwardThread = vi.fn();
    renderList({}, false, "Inbox", { onReplyThread, onForwardThread });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Reply", hidden: true }),
    );
    expect(onReplyThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
    );

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Forward", hidden: true }),
    );
    expect(onForwardThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
    );
  });

  it("runs star from the thread context menu", async () => {
    const onToggleStarThread = vi.fn();
    renderList({}, false, "Inbox", { onToggleStarThread });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Star", hidden: true }),
    );

    expect(onToggleStarThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
      true,
    );
  });

  it("offers Mark as read for unread conversations", async () => {
    const onToggleReadThread = vi.fn();
    renderList({}, false, "Inbox", { onToggleReadThread });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", {
        name: "Mark as read",
        hidden: true,
      }),
    );

    expect(onToggleReadThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
      true,
    );
  });

  it("offers Mark as unread for read conversations", async () => {
    const onToggleReadThread = vi.fn();
    renderList({}, false, "Inbox", {
      messages: [{ ...messages[0], is_read: true }],
      onToggleReadThread,
    });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", {
        name: "Mark as unread",
        hidden: true,
      }),
    );

    expect(onToggleReadThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
      false,
    );
  });

  it("offers Delete from the context menu", async () => {
    const onActionThread = vi.fn();
    renderList({}, false, "Inbox", { onActionThread });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Delete", hidden: true }),
    );

    expect(onActionThread).toHaveBeenCalledWith(
      expect.objectContaining({ id: "account:1" }),
      "trash",
    );
  });

  it("offers Unstar and Not spam for a starred spam conversation", async () => {
    renderList({}, false, "Spam", {
      messages: [
        {
          ...messages[0],
          mailbox: "Spam",
          is_flagged: true,
        },
      ],
    });

    fireEvent.contextMenu(screen.getByText("Subject 1").closest("button")!);

    expect(
      await screen.findByRole("menuitem", { name: "Unstar", hidden: true }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: "Not spam", hidden: true }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: "Archive", hidden: true }),
    ).toBeDisabled();
  });
});
