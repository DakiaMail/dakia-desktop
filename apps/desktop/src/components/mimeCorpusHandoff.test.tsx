import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { MailSummary, MessageContent } from "../types";
import handoff from "../../../../testdata/mime-reader-handoff/ambiguous-inline-identities.json";
import corpusSemantics from "../../../../testdata/mime-reader-handoff/corpus-semantics.json";
import truncatedTolerance from "../../../../testdata/mime-reader-handoff/truncated-multipart-lf-representation-tolerance.json";
import { Reader } from "./Reader";

const message: MailSummary = {
  id: "11111111-1111-4111-8111-111111111111:INBOX:42",
  account_id: "11111111-1111-4111-8111-111111111111",
  mailbox: "INBOX",
  uid: 42,
  mailbox_uid_validity: 77,
  thread_id: "mime-corpus-thread",
  subject: handoff.readerMessage.subject,
  from_address: "sender@example.test",
  to_addresses: "recipient@example.test",
  received_at: "2026-07-28T12:04:00Z",
  snippet: handoff.readerMessage.snippet,
  body_text: "headers-only placeholder",
  is_read: true,
  is_flagged: false,
  has_attachments: handoff.readerMessage.hasAttachments,
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

function rendererSource(host: HTMLElement) {
  const frame = host.shadowRoot?.querySelector("iframe");
  return frame?.getAttribute("srcdoc") ?? "";
}

function rendererText(host: HTMLElement) {
  return new DOMParser().parseFromString(rendererSource(host), "text/html").body
    .textContent;
}

type CorpusScenario = {
  bodyTextIncludes: string[];
  bodyTextExcludes?: string[];
  bodyHtmlIncludes?: string[];
  downloadableFilenames: string[];
  readerText: string;
  readerBodyHtml?: string;
};

const readerScenarios = Object.entries(
  corpusSemantics.scenarios as Record<string, CorpusScenario>,
).map(([fixtureId, scenario]) => ({ fixtureId, ...scenario }));

type TruncatedRepresentation = {
  bodyText: string;
  bodyHtml: string | null;
  downloadableFilenames: string[];
};

type TruncatedTolerance = {
  fixtureId: string;
  readerText: string;
  forbiddenText: string;
  full: TruncatedRepresentation;
  selective: TruncatedRepresentation;
};

const malformedTruncatedTolerance = truncatedTolerance as TruncatedTolerance;

function truncatedRepresentationContent(
  representation: TruncatedRepresentation,
): MessageContent {
  return {
    body_text: representation.bodyText,
    body_html: representation.bodyHtml ?? undefined,
    attachments: representation.downloadableFilenames.map(
      (filename, index) => ({
        id: `truncated-multipart-lf:attachment:${index + 1}`,
        message_id: "mime-corpus-truncated-multipart-lf",
        filename,
        mime_type: "application/octet-stream",
        size_bytes: index + 1,
        is_inline: false,
        presentation: "downloadable",
        is_potentially_unsafe: false,
      }),
    ),
  };
}

function scenarioMessage(
  fixtureId: string,
  scenario: CorpusScenario,
): MailSummary {
  return {
    ...message,
    id: `mime-corpus-${fixtureId}`,
    subject: `Governed MIME ${fixtureId}`,
    snippet: scenario.readerText,
    has_attachments: scenario.downloadableFilenames.length > 0,
  };
}

function scenarioContent(
  fixtureId: string,
  scenario: CorpusScenario,
): MessageContent {
  const messageId = `mime-corpus-${fixtureId}`;
  return {
    body_text: scenario.bodyTextIncludes.join("\n"),
    body_html: scenario.readerBodyHtml,
    attachments: scenario.downloadableFilenames.map((filename, index) => ({
      id: `${messageId}:attachment:${index + 1}`,
      message_id: messageId,
      filename,
      mime_type: "application/octet-stream",
      size_bytes: index + 1,
      is_inline: false,
      presentation: "downloadable",
      is_potentially_unsafe: false,
    })),
  };
}

describe("governed MIME corpus Reader handoff", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(api, "content").mockResolvedValue(
      handoff.messageContent as MessageContent,
    );
    vi.spyOn(api, "saveAttachment").mockResolvedValue("~/Downloads/logo.png");
  });

  it("keeps ambiguous resources unembedded and offers both declared downloads", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(await screen.findByText("logo.png")).toBeVisible();
    expect(screen.getByText("source.pdf")).toBeVisible();
    const renderedMessage = await screen.findByRole("document", {
      name: handoff.readerMessage.subject,
    });
    await waitFor(() =>
      expect(rendererSource(renderedMessage)).toContain("Fictional resources"),
    );
    const source = rendererSource(renderedMessage);
    expect(source).not.toContain("data:image/");
    expect(source.match(/<img>/g)).toHaveLength(2);
    expect(source).not.toContain("<img src=");

    fireEvent.click(
      screen.getByRole("button", { name: "Save logo.png to Downloads" }),
    );
    await waitFor(() =>
      expect(api.saveAttachment).toHaveBeenCalledWith(
        message.id,
        handoff.messageContent.attachments[0].id,
        message.mailbox_uid_validity,
      ),
    );
  });

  it.each([
    ["complete", malformedTruncatedTolerance.full],
    ["selective", malformedTruncatedTolerance.selective],
  ] as const)(
    "renders the declared malformed truncated MIME $0 representation without boundary leakage",
    async (_representationName, representation) => {
      const scenario: CorpusScenario = {
        bodyTextIncludes: [malformedTruncatedTolerance.readerText],
        downloadableFilenames: representation.downloadableFilenames,
        readerText: malformedTruncatedTolerance.readerText,
        readerBodyHtml: representation.bodyHtml ?? undefined,
      };
      const scenarioMessageValue = scenarioMessage(
        `${malformedTruncatedTolerance.fixtureId}-${_representationName}`,
        scenario,
      );
      vi.mocked(api.content).mockResolvedValue(
        truncatedRepresentationContent(representation),
      );
      render(
        <MantineProvider>
          <Reader {...props} message={scenarioMessageValue} />
        </MantineProvider>,
      );

      if (representation.bodyHtml) {
        const renderedMessage = await screen.findByRole("document", {
          name: scenarioMessageValue.subject,
        });
        await waitFor(() =>
          expect(rendererText(renderedMessage)).toContain(
            malformedTruncatedTolerance.readerText,
          ),
        );
        expect(rendererSource(renderedMessage)).not.toContain(
          malformedTruncatedTolerance.forbiddenText,
        );
      } else {
        expect(
          await screen.findByText(
            /The second leaf and closing boundary are deliberately missing/,
          ),
        ).toBeVisible();
        expect(
          screen.queryByText(malformedTruncatedTolerance.forbiddenText),
        ).toBeNull();
      }
      for (const filename of representation.downloadableFilenames) {
        expect(screen.getByText(filename)).toBeVisible();
      }
    },
  );

  it.each(readerScenarios)(
    "renders the declared $fixtureId corpus semantics without hidden MIME children",
    async ({ fixtureId, ...scenario }) => {
      const scenarioMessageValue = scenarioMessage(fixtureId, scenario);
      vi.mocked(api.content).mockResolvedValue(
        scenarioContent(fixtureId, scenario),
      );
      render(
        <MantineProvider>
          <Reader {...props} message={scenarioMessageValue} />
        </MantineProvider>,
      );

      const renderedMessage = scenario.readerBodyHtml
        ? await screen.findByRole("document", {
            name: scenarioMessageValue.subject,
          })
        : null;
      const readerText = renderedMessage
        ? () => rendererText(renderedMessage)
        : () => screen.getByText(scenario.readerText).textContent;
      await waitFor(() => expect(readerText()).toContain(scenario.readerText));
      for (const filename of scenario.downloadableFilenames) {
        expect(screen.getByText(filename)).toBeVisible();
      }
      for (const excluded of scenario.bodyTextExcludes ?? []) {
        expect(readerText()).not.toContain(excluded);
      }
      if (scenario.readerBodyHtml?.includes("data:image/png")) {
        expect(rendererSource(renderedMessage!)).toContain(
          "data:image/png;base64,iVBORw0KGgo=",
        );
      }
    },
  );
});
