import { MantineProvider } from "@mantine/core";
import { render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { MailSummary, MessageContent } from "../types";
import { Reader } from "./Reader";

type ArtifactMessage = {
  fixtureId: string;
  message: MailSummary;
  content: MessageContent;
};

type Artifact = {
  schemaVersion: number;
  nonce: string;
  messages: ArtifactMessage[];
};

const nodeEnv = (
  globalThis as typeof globalThis & {
    process?: { env?: Record<string, string | undefined> };
  }
).process?.env;
const expectedNonce =
  import.meta.env.VITE_DAKIA_MIME_READER_ARTIFACT_NONCE ||
  nodeEnv?.VITE_DAKIA_MIME_READER_ARTIFACT_NONCE;
const encodedArtifact =
  import.meta.env.VITE_DAKIA_MIME_READER_TAURI_ARTIFACT ||
  nodeEnv?.VITE_DAKIA_MIME_READER_TAURI_ARTIFACT;
function decodeArtifact(encoded: string): Artifact {
  const bytes = Uint8Array.from(atob(encoded), (value) => value.charCodeAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
}
const artifact: Artifact | null = encodedArtifact
  ? decodeArtifact(encodedArtifact)
  : null;

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

function rendererSource(host: HTMLElement) {
  return host.shadowRoot?.querySelector("iframe")?.getAttribute("srcdoc") ?? "";
}

const readerTextByFixture: Record<string, string> = {
  "competing-nested-alternatives": "Visible <strong>HTML</strong> body.",
  "attached-message-rfc822": "fully redacted",
  "multipart-attachment-container": "Visible parent body.",
  "empty-alternative-leaf": "Visible HTML after an empty plain part.",
  "truncated-multipart-lf":
    "second leaf and closing boundary are deliberately missing.",
  "charset-matrix": "Zażółć",
  "format-flowed-delsp": "This is the HTML alternative.",
  "international-reader-semantics": "مرحبا بالعالم",
  "ambiguous-inline-identities": "Fictional resources",
  "provider-signature-inline":
    "Redacted message content for a fictional insurance case.",
  "filename-parameters-and-encodings": "Synthetic filename coverage.",
  "linkedin-inline-content-id": "Redacted HTML=content with a foldedline.",
  "standards-unknown-transfer-encoding": "Visible safe body.",
};

describe.skipIf(!artifact || !expectedNonce)(
  "real MIME profile through Tauri message_content into Reader",
  () => {
    beforeEach(() => {
      vi.restoreAllMocks();
      expect(artifact?.schemaVersion).toBe(1);
      expect(artifact?.nonce).toBe(expectedNonce);
      vi.spyOn(api, "content").mockImplementation(async (messageId) => {
        const item = artifact!.messages.find(
          (candidate) => candidate.message.id === messageId,
        );
        if (!item) throw new Error(`missing artifact message: ${messageId}`);
        return item.content;
      });
    });

    it.each(artifact?.messages ?? [])(
      "renders the actual $fixtureId Tauri payload",
      async ({ fixtureId, message, content }) => {
        render(
          <MantineProvider>
            <Reader {...props} message={message} />
          </MantineProvider>,
        );

        await waitFor(() =>
          expect(api.content).toHaveBeenCalledWith(
            message.id,
            message.mailbox_uid_validity,
          ),
        );
        for (const attachment of content.attachments) {
          expect(screen.getByText(attachment.filename)).toBeVisible();
        }

        if (fixtureId === "attached-message-rfc822") {
          expect(content.body_html).toBeNull();
          expect(await screen.findByText(/fully redacted/)).toBeVisible();
          expect(
            screen.queryByText(/Original plain text/),
          ).not.toBeInTheDocument();
        } else if (fixtureId === "multipart-attachment-container") {
          expect(content.body_html).toBeNull();
          expect(await screen.findByText(/Visible parent body/)).toBeVisible();
          expect(
            screen.queryByText(/Secret child text/),
          ).not.toBeInTheDocument();
        } else if (fixtureId === "charset-matrix") {
          expect(content.body_html).toBeNull();
          expect(
            await screen.findByText(readerTextByFixture[fixtureId]),
          ).toBeVisible();
          expect(
            screen.queryByText(/Unknown charset must not become invented text/),
          ).not.toBeInTheDocument();
        } else if (fixtureId === "filename-parameters-and-encodings") {
          expect(content.body_html).toBeNull();
          expect(
            await screen.findByText("Synthetic filename coverage."),
          ).toBeVisible();
          const expectedFilenames = [
            "quarterly report final.pdf",
            "report-test.txt",
            "Prügimaja plaan vaated 31.08.pdf",
            "Prügimaja_31.08.pdf",
            "Prügimaja.pdf",
            "invoice;final.pdf",
            "invoicegpj.exe",
            "broken%ZZname.pdf",
          ];
          expect(
            content.attachments.map((attachment) => attachment.filename),
          ).toEqual(expectedFilenames);
          expect(
            screen.getAllByRole("button", {
              name: /^Save .* to Downloads$/,
            }),
          ).toHaveLength(expectedFilenames.length);
          for (const filename of expectedFilenames) {
            expect(screen.getByText(filename)).toBeVisible();
            expect(
              screen.getByRole("button", {
                name: `Save ${filename} to Downloads`,
              }),
            ).toBeVisible();
          }
          expect(
            content.attachments.filter(
              (attachment) => attachment.is_potentially_unsafe,
            ),
          ).toEqual([expect.objectContaining({ filename: "invoicegpj.exe" })]);
          const unsafeRow = screen
            .getByText("invoicegpj.exe")
            .closest(".attachment-row");
          expect(unsafeRow).toBeInstanceOf(HTMLElement);
          if (!(unsafeRow instanceof HTMLElement)) {
            throw new Error("unsafe attachment row is not an HTML element");
          }
          expect(
            within(unsafeRow).getByText(
              "This file may run software. Only open it if you trust the sender.",
            ),
          ).toBeVisible();
        } else if (fixtureId === "standards-unknown-transfer-encoding") {
          expect(content.body_html).toBeNull();
          expect(await screen.findByText("Visible safe body.")).toBeVisible();
          expect(
            screen.queryByRole("document", { name: message.subject }),
          ).not.toBeInTheDocument();
          expect(
            screen.queryByText(/Opaque transport bytes/),
          ).not.toBeInTheDocument();
          expect(
            screen.queryByText(/opaque HTML transport/),
          ).not.toBeInTheDocument();
          expect(content.attachments).toEqual([
            expect.objectContaining({
              filename: "invoice.exe",
              mime_type: "application/octet-stream",
              size_kind: "encoded",
              is_potentially_unsafe: true,
            }),
            expect.objectContaining({
              filename: "attachment",
              mime_type: "application/octet-stream",
              size_kind: "encoded",
              is_potentially_unsafe: false,
            }),
          ]);
          expect(
            screen.getAllByRole("button", {
              name: /^Save .* to Downloads$/,
            }),
          ).toHaveLength(2);
          expect(
            screen.getByRole("button", {
              name: "Save invoice.exe to Downloads",
            }),
          ).toBeVisible();
          expect(
            screen.getByRole("button", {
              name: "Save attachment to Downloads",
            }),
          ).toBeVisible();
          const unsafeRow = screen
            .getByText("invoice.exe")
            .closest(".attachment-row");
          const safeRow = screen
            .getByText("attachment")
            .closest(".attachment-row");
          expect(unsafeRow).toBeInstanceOf(HTMLElement);
          expect(safeRow).toBeInstanceOf(HTMLElement);
          if (
            !(unsafeRow instanceof HTMLElement) ||
            !(safeRow instanceof HTMLElement)
          ) {
            throw new Error("attachment row is not an HTML element");
          }
          expect(
            within(unsafeRow).getByText(
              "This file may run software. Only open it if you trust the sender.",
            ),
          ).toBeVisible();
          expect(
            within(safeRow).queryByText(
              "This file may run software. Only open it if you trust the sender.",
            ),
          ).not.toBeInTheDocument();
        } else {
          const document = await screen.findByRole("document", {
            name: message.subject,
          });
          await waitFor(() => expect(rendererSource(document)).not.toBe(""));
          const source = rendererSource(document);
          expect(source).toContain(readerTextByFixture[fixtureId]);
          if (fixtureId === "truncated-multipart-lf") {
            expect(content.body_text).toContain("first leaf is complete");
            expect(source).toContain(
              "second leaf and closing boundary are deliberately missing.",
            );
            expect(source).not.toContain("--truncated");
          } else if (fixtureId === "competing-nested-alternatives") {
            expect(source).toContain("Visible <strong>HTML</strong> body.");
            expect(source).toContain("data:image/png;base64,iVBORw0KGgo=");
            expect(source).not.toContain("redacted-notes");
          } else if (fixtureId === "empty-alternative-leaf") {
            expect(source).toContain("Visible HTML after an empty plain part.");
          } else if (fixtureId === "format-flowed-delsp") {
            expect(source).toContain("This is the HTML alternative.");
            expect(content.body_text).toContain("soft spacethat continues");
            expect(content.body_text).toContain(
              "> Quoted flowed textcontinues too.",
            );
          } else if (fixtureId === "ambiguous-inline-identities") {
            expect(source).toContain("Fictional resources");
            expect(source).not.toContain("data:image/");
            expect(source.match(/<img>/g)).toHaveLength(2);
            expect(source).not.toContain("<img src=");
          } else if (fixtureId === "international-reader-semantics") {
            expect(message.subject).toBe("Fictional 🌍 reader message");
            expect(source).toContain("Tere, maailma 👋");
            expect(source).toContain("مرحبا بالعالم");
            expect(source).toContain("שלום עולם");
            expect(source).toContain('dir="rtl"');
            expect(source).toContain("Fictional signature");
          } else if (fixtureId === "provider-signature-inline") {
            expect(source).toContain(
              "Redacted message content for a fictional insurance case.",
            );
            expect(source).toContain("data:image/png;base64,iVBORw0KGgo=");
            expect(source).not.toContain("cid:image001.png@redacted");
            expect(
              content.attachments.map((attachment) => attachment.filename),
            ).toEqual(["claim-documents.pdf"]);
            expect(screen.getByText("claim-documents.pdf")).toBeVisible();
            expect(screen.queryByText("image001.png")).not.toBeInTheDocument();
            expect(
              screen.getAllByRole("button", {
                name: /^Save .* to Downloads$/,
              }),
            ).toHaveLength(1);
            expect(
              screen.getByRole("button", {
                name: "Save claim-documents.pdf to Downloads",
              }),
            ).toBeVisible();
          } else if (fixtureId === "linkedin-inline-content-id") {
            expect(content.body_text.trim()).toBe(
              "Redacted plain text=content with a foldedline.",
            );
            expect(source).toContain(
              "Redacted HTML=content with a foldedline.",
            );
            expect(content.attachments).toEqual([]);
            expect(
              screen.queryAllByRole("button", {
                name: /^Save .* to Downloads$/,
              }),
            ).toHaveLength(0);
          }
        }
      },
    );
  },
);
