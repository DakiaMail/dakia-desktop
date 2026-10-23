import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { MailSummary, MessageContent } from "../types";
import { Reader } from "./Reader";

type Artifact = {
  schemaVersion: number;
  nonce: string;
  message: MailSummary;
  content: MessageContent;
  individual: { path: string; bytesBase64: string; unsafe: boolean };
  bulk: {
    successfulPaths: string[];
    error: { kind: "partial"; savedCount: number; totalCount: number };
    partialPublishedCount: number;
  };
};

const nodeEnv = (
  globalThis as typeof globalThis & {
    process?: { env?: Record<string, string | undefined> };
  }
).process?.env;
const expectedNonce =
  import.meta.env.VITE_DAKIA_ATTACHMENT_COMMAND_ARTIFACT_NONCE ||
  nodeEnv?.VITE_DAKIA_ATTACHMENT_COMMAND_ARTIFACT_NONCE;
const encodedArtifact =
  import.meta.env.VITE_DAKIA_ATTACHMENT_COMMAND_TAURI_ARTIFACT ||
  nodeEnv?.VITE_DAKIA_ATTACHMENT_COMMAND_TAURI_ARTIFACT;

function decodeArtifact(encoded: string): Artifact {
  const bytes = Uint8Array.from(atob(encoded), (value) => value.charCodeAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
}

const artifact = encodedArtifact ? decodeArtifact(encodedArtifact) : null;

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

describe.skipIf(!artifact || !expectedNonce)(
  "real attachment command artifact in Reader",
  () => {
    beforeEach(() => {
      vi.restoreAllMocks();
      expect(artifact?.schemaVersion).toBe(1);
      expect(artifact?.nonce).toBe(expectedNonce);
      vi.spyOn(api, "content").mockResolvedValue(artifact!.content);
      vi.spyOn(api, "saveAttachment").mockResolvedValue(
        artifact!.individual.path,
      );
      vi.spyOn(api, "saveAllAttachments")
        .mockResolvedValueOnce(artifact!.bulk.successfulPaths)
        .mockRejectedValueOnce(JSON.stringify(artifact!.bulk.error));
    });

    it("keeps the actual unsafe warning through individual, successful, and partial bulk saves", async () => {
      const unsafe = artifact!.content.attachments.find(
        (attachment) => attachment.is_potentially_unsafe,
      );
      expect(unsafe?.filename).toBe("quarterly-report.pdf.command");
      expect(artifact!.individual.unsafe).toBe(true);
      expect(artifact!.individual.bytesBase64).toBe(
        "ZmljdGlvbmFsIGF0dGFjaG1lbnQgYnl0ZXMA/w==",
      );
      expect(artifact!.bulk.error).toEqual({
        kind: "partial",
        savedCount: 1,
        totalCount: 2,
      });
      expect(artifact!.bulk.successfulPaths).toHaveLength(2);
      expect(artifact!.bulk.partialPublishedCount).toBe(1);

      render(
        <MantineProvider>
          <Reader {...props} message={artifact!.message} />
        </MantineProvider>,
      );

      expect(await screen.findByText(unsafe!.filename)).toBeVisible();
      expect(
        screen.getByText(
          "This file may run software. Only open it if you trust the sender.",
        ),
      ).toBeVisible();

      fireEvent.click(
        screen.getByRole("button", {
          name: `Save ${unsafe!.filename} to Downloads`,
        }),
      );
      await waitFor(() =>
        expect(api.saveAttachment).toHaveBeenCalledWith(
          artifact!.message.id,
          unsafe!.id,
          artifact!.message.mailbox_uid_validity,
        ),
      );
      expect(
        await screen.findByText("Saved 1 attachment to Downloads"),
      ).toBeVisible();
      expect(
        screen.getByText(
          "This file may run software. Only open it if you trust the sender.",
        ),
      ).toBeVisible();

      fireEvent.click(screen.getByRole("button", { name: "Save all" }));
      await waitFor(() =>
        expect(api.saveAllAttachments).toHaveBeenCalledWith(
          artifact!.message.id,
          artifact!.message.mailbox_uid_validity,
        ),
      );
      expect(
        await screen.findByText("Saved 2 attachments to Downloads"),
      ).toBeVisible();
      expect(
        screen.getByText(
          "This file may run software. Only open it if you trust the sender.",
        ),
      ).toBeVisible();

      fireEvent.click(screen.getByRole("button", { name: "Save all" }));
      await waitFor(() =>
        expect(api.saveAllAttachments).toHaveBeenCalledTimes(2),
      );
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
  },
);
