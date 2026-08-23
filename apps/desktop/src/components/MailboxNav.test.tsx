import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "../i18n";
import type { Account } from "../types";
import { MailboxNav } from "./MailboxNav";

const account: Account = {
  id: "account-1",
  email: "me@example.com",
  account_name: "Personal mail",
  display_name: "Me",
  provider_id: "test",
  auth: { type: "password", username: "me@example.com" },
  imap_host: "imap.example.com",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.example.com",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};

describe("MailboxNav accounts", () => {
  it("shows the local account name and opens its context menu on right click", () => {
    const onAccountContextMenu = vi.fn();
    render(
      <MailboxNav
        accounts={[account]}
        mailbox="INBOX"
        onSelectAccount={vi.fn()}
        onAccountContextMenu={onAccountContextMenu}
        onAddAccount={vi.fn()}
        onMailbox={vi.fn()}
        onFeedback={vi.fn()}
      />,
    );

    const row = screen.getByRole("button", { name: "Personal mail" });
    expect(row).toHaveAttribute("title", "me@example.com");
    fireEvent.contextMenu(row);
    expect(onAccountContextMenu).toHaveBeenCalledWith(account);
  });

  it("shows the number of messages currently sending in Outbox", () => {
    render(
      <MailboxNav
        accounts={[account]}
        mailbox="INBOX"
        outboxCount={2}
        onSelectAccount={vi.fn()}
        onAccountContextMenu={vi.fn()}
        onAddAccount={vi.fn()}
        onMailbox={vi.fn()}
        onFeedback={vi.fn()}
      />,
    );

    expect(screen.getByRole("button", { name: /Outbox/ })).toHaveTextContent(
      "2",
    );
    expect(screen.getByLabelText("2 emails sending")).toBeVisible();
  });

  it("shows the scoped starred conversation count", () => {
    render(
      <MailboxNav
        accounts={[account]}
        mailbox="INBOX"
        starredCount={7}
        onSelectAccount={vi.fn()}
        onAccountContextMenu={vi.fn()}
        onAddAccount={vi.fn()}
        onMailbox={vi.fn()}
        onFeedback={vi.fn()}
      />,
    );
    expect(screen.getByLabelText("7 starred conversations")).toBeVisible();
  });

  it("keeps feedback in a separate footer and invokes its callback", () => {
    const onFeedback = vi.fn();
    const { container } = render(
      <MailboxNav
        accounts={[account]}
        mailbox="INBOX"
        onSelectAccount={vi.fn()}
        onAccountContextMenu={vi.fn()}
        onAddAccount={vi.fn()}
        onMailbox={vi.fn()}
        onFeedback={onFeedback}
      />,
    );

    const feedback = screen.getByRole("button", { name: "Feedback" });
    expect(feedback.closest(".mailbox-nav-footer")).toBe(
      container.querySelector(".mailbox-nav-footer"),
    );
    fireEvent.click(feedback);
    expect(onFeedback).toHaveBeenCalledOnce();
  });

  it("disables feedback while sender accounts are still loading", () => {
    render(
      <MailboxNav
        accounts={[]}
        mailbox="INBOX"
        onSelectAccount={vi.fn()}
        onAccountContextMenu={vi.fn()}
        onAddAccount={vi.fn()}
        onMailbox={vi.fn()}
        onFeedback={vi.fn()}
        feedbackDisabled
      />,
    );

    expect(screen.getByRole("button", { name: "Feedback" })).toBeDisabled();
  });
});
