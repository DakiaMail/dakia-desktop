import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { api } from "./api";
import { Reader } from "./components/Reader";
import type { MailSummary } from "./types";

const message: MailSummary = {
  id: "fictional-unsafe-attachment-message",
  account_id: "fictional-attachment-account",
  mailbox: "INBOX",
  mailbox_uid_validity: 77,
  uid: 42,
  thread_id: "fictional-unsafe-attachment-thread",
  subject: "Fictional attachment safety message",
  from_address: "sender@example.test",
  to_addresses: "reader@example.test",
  received_at: "2026-10-17T12:00:00Z",
  snippet: "Fictional attachment only.",
  body_text: "Fictional attachment only.",
  is_read: true,
  is_flagged: false,
  has_attachments: true,
};

const props = {
  aiLoading: false,
  aiConnected: false,
  actionsDisabled: false,
  onArchive: vi.fn(),
  onSpam: vi.fn(),
  onTrash: vi.fn(),
  onPermanentDelete: vi.fn(async (): Promise<void> => undefined),
  onReply: vi.fn(),
  onReplyAll: vi.fn(),
  onForward: vi.fn(),
  onSendAgain: vi.fn(),
  onToggleRead: vi.fn(),
  onSummarize: vi.fn(),
  onCopyAi: vi.fn(),
  onComposeTo: vi.fn(),
  onComposeLink: vi.fn(),
  onAddressContextMenu: vi.fn(),
  unsubscribeLoading: false,
  onUnsubscribe: vi.fn(),
  onToggleStar: vi.fn(),
};

describe("unsafe email attachment boundary", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(api, "content").mockResolvedValue({
      body_text: "Fictional attachment only.",
      attachments: [
        {
          id: "fictional-command",
          message_id: message.id,
          filename: "invoice.command",
          mime_type: "application/octet-stream",
          size_bytes: 15,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: true,
        },
      ],
    });
    vi.spyOn(api, "saveAttachment").mockResolvedValue(
      "/fictional/Downloads/invoice.command",
    );
  });

  it("keeps the unsafe warning visible through the Reader and saves the exact epoch-bound attachment", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(await screen.findByText("invoice.command")).toBeVisible();
    expect(
      screen.getByText(
        "This file may run software. Only open it if you trust the sender.",
      ),
    ).toBeVisible();

    fireEvent.click(
      screen.getByRole("button", {
        name: "Save invoice.command to Downloads",
      }),
    );
    await waitFor(() =>
      expect(api.saveAttachment).toHaveBeenCalledWith(
        message.id,
        "fictional-command",
        77,
      ),
    );
    expect(
      await screen.findByText("Saved 1 attachment to Downloads"),
    ).toBeVisible();
  });

  it("reports a partial Save all outcome without hiding the file that was already saved", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Two fictional attachments.",
      attachments: [
        {
          id: "first-fictional-file",
          message_id: message.id,
          filename: "first.command",
          mime_type: "application/octet-stream",
          size_bytes: 11,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: true,
        },
        {
          id: "second-fictional-file",
          message_id: message.id,
          filename: "second.pdf",
          mime_type: "application/pdf",
          size_bytes: 12,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
      ],
    });
    vi.spyOn(api, "saveAllAttachments").mockRejectedValue(
      JSON.stringify({ kind: "partial", savedCount: 1, totalCount: 2 }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(await screen.findByText("first.command")).toBeVisible();
    expect(
      screen.getByText(
        "This file may run software. Only open it if you trust the sender.",
      ),
    ).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Save all" }));
    expect(
      await screen.findByText(
        "Saved 1 of 2 attachments. Could not save the remaining attachments.",
      ),
    ).toBeVisible();
    expect(
      screen.getByText(
        "This file may run software. Only open it if you trust the sender.",
      ),
    ).toBeVisible();
  });
});
