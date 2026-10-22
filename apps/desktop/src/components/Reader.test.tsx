import {
  act,
  cleanup,
  createEvent,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { format } from "date-fns";
import "../i18n";
import { api, MessageContentError } from "../api";
import freshdeskReplySection from "../test/fixtures/freshdesk-reply-section.html?raw";
import type { MailSummary, MessageContent } from "../types";
import highRiskContract from "../../testdata/tauri-contracts/high-risk.json";
import { Reader } from "./Reader";

const translationMocks = vi.hoisted(() => ({
  detect: vi.fn(),
  translate: vi.fn(),
  confirm: vi.fn(),
  show: vi.fn(),
}));

vi.mock("../offlineTranslation", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../offlineTranslation")>()),
  detectTranslationLanguage: translationMocks.detect,
  translateOffline: translationMocks.translate,
}));

vi.mock("../nativeFeedback", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../nativeFeedback")>()),
  confirmNativeAction: translationMocks.confirm,
  showNativeMessage: translationMocks.show,
}));

const message: MailSummary = {
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 1,
  thread_id: "thread-1",
  subject: "Weekly notes",
  from_address: "list@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Message body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
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
  // jsdom does not create a browsing context for an iframe in a shadow tree.
  // This stand-in preserves the production source check in component tests.
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

describe("Reader unsubscribe action", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    translationMocks.detect.mockResolvedValue({
      language: "et",
      languageName: "Estonian",
      reliable: true,
    });
    translationMocks.translate.mockImplementation(
      async (_source: string, text: string) => `EN: ${text}`,
    );
    translationMocks.confirm.mockResolvedValue(true);
    translationMocks.show.mockResolvedValue(undefined);
    vi.spyOn(api, "content").mockResolvedValue({
      body_text: "Tere maailm",
      attachments: [],
    });
    vi.spyOn(api, "hydrateMessage").mockResolvedValue(message);
    vi.spyOn(api, "translationModels").mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: true,
      },
    ]);
    vi.spyOn(api, "installTranslationModel").mockResolvedValue({
      source: "et",
      target: "en",
      modelPath: "/model",
      shortlistPath: "/shortlist",
      vocabPaths: ["/vocab"],
      config: {},
    });
    vi.spyOn(api, "cancelTranslationModelInstall").mockResolvedValue();
    vi.spyOn(api, "exportMessage").mockResolvedValue(
      "/Users/alex/Downloads/weekly-notes.eml",
    );
  });

  it("keeps AI controls and results hidden even when connected", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={message}
          aiConnected
          aiResult="Hidden summary"
        />
      </MantineProvider>,
    );

    expect(screen.queryByRole("button", { name: "Summarize" })).toBeNull();
    expect(screen.queryByText("Hidden summary")).toBeNull();
  });

  it("renders a valid RFC 3339 leap second at the final displayable instant", () => {
    const receivedAt = "2016-12-31T23:59:60+00:00";
    const expected = format(
      new Date("2016-12-31T23:59:59.999+00:00"),
      "EEEE d MMM 'at' HH:mm",
    );

    expect(() =>
      render(
        <MantineProvider>
          <Reader
            {...props}
            message={{ ...message, received_at: receivedAt }}
          />
        </MantineProvider>,
      ),
    ).not.toThrow();

    expect(screen.getByText(expected)).toBeVisible();
    expect(receivedAt).toBe("2016-12-31T23:59:60+00:00");
  });

  it("shows an unknown-date label for malformed timestamps instead of crashing", () => {
    expect(() =>
      render(
        <MantineProvider>
          <Reader
            {...props}
            message={{ ...message, received_at: "2016-02-30T23:59:60+00:00" }}
          />
        </MantineProvider>,
      ),
    ).not.toThrow();

    expect(screen.getByText("Unknown date")).toBeVisible();
  });

  it("shows unsubscribe only for a supported message", () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    expect(screen.queryByRole("button", { name: "Unsubscribe" })).toBeNull();

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, unsubscribe_kind: "one_click" }}
        />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    expect(props.onUnsubscribe).toHaveBeenCalledOnce();
  });

  it("opens email-body mailto links in Dakia compose with the message account context", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Write to Juhan",
      body_html:
        '<p><a href="mailto:juhan%2Btamm@example.com?subject=Tere&amp;body=Kohtume%20homme">Write to Juhan</a></p>',
      attachments: [],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    const documentRole = await screen.findByRole("document", {
      name: "Weekly notes",
    });
    const anchor = await waitFor(() => {
      const nextAnchor =
        emailRendererDocument(documentRole)?.querySelector("a");
      expect(nextAnchor).toBeTruthy();
      return nextAnchor as HTMLAnchorElement;
    });
    sendEmailRendererMessage(documentRole, {
      channel: "dakia-email-renderer",
      token: emailRendererToken(documentRole),
      type: "external-link",
      href: anchor.getAttribute("data-dakia-external-href"),
    });

    expect(props.onComposeLink).toHaveBeenCalledWith(message, {
      to: "juhan+tamm@example.com",
      subject: "Tere",
      body: "Kohtume homme",
    });
  });

  it("loads remote content only after a per-message Reader action and resets it when mailbox identity rolls over", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Newsletter",
      body_html:
        '<img src="https://images.example.test/tracker.png" alt="Tracking image"><style>.hero { background: url("https://images.example.test/hero.jpg") }</style><p class="hero">Newsletter</p>',
      attachments: [],
    });
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, mailbox_uid_validity: 77 }} />
      </MantineProvider>,
    );
    const emailRoot = () =>
      emailRendererDocument(
        screen.getByRole("document", { name: "Weekly notes" }),
      );

    await waitFor(() =>
      expect(emailRoot()?.querySelector("img")?.hasAttribute("src")).toBe(
        false,
      ),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Load remote content" }),
    );
    await waitFor(() =>
      expect(emailRoot()?.querySelector("img")?.getAttribute("src")).toBe(
        "https://images.example.test/tracker.png",
      ),
    );

    rerender(
      <MantineProvider>
        <Reader {...props} message={{ ...message, mailbox_uid_validity: 78 }} />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(emailRoot()?.querySelector("img")?.hasAttribute("src")).toBe(
        false,
      ),
    );
    expect(
      screen.getByRole("button", { name: "Load remote content" }),
    ).toBeVisible();
  });

  it("does not revive remote-content consent after switching between mounted thread messages", async () => {
    const earlier = {
      ...message,
      id: "remote-earlier",
      uid: 11,
      subject: "Remote earlier",
      from_name: "Earlier Sender",
      received_at: "2026-07-19T08:00:00Z",
    };
    const latest = {
      ...message,
      id: "remote-latest",
      uid: 12,
      subject: "Remote latest",
      from_name: "Latest Sender",
      received_at: "2026-07-19T09:00:00Z",
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text:
        id === earlier.id ? "Earlier remote body" : "Latest remote body",
      body_html:
        id === earlier.id
          ? '<img alt="Earlier remote image" src="https://images.example.test/earlier.png">'
          : '<img alt="Latest remote image" src="https://images.example.test/latest.png">',
      attachments: [],
    }));
    render(
      <MantineProvider>
        <Reader {...props} message={earlier} messages={[earlier, latest]} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Earlier Sender",
      }),
    );
    const earlierDocument = await screen.findByRole("document", {
      name: "Remote earlier",
    });
    await waitFor(() =>
      expect(
        emailRendererDocument(earlierDocument)
          ?.querySelector("img")
          ?.hasAttribute("src"),
      ).toBe(false),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Load remote content" }),
    );
    await waitFor(() =>
      expect(
        emailRendererDocument(earlierDocument)
          ?.querySelector("img")
          ?.getAttribute("src"),
      ).toBe("https://images.example.test/earlier.png"),
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Latest Sender",
      }),
    );
    await screen.findByRole("document", { name: "Remote latest" });
    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Earlier Sender",
      }),
    );
    const reopenedEarlierDocument = await screen.findByRole("document", {
      name: "Remote earlier",
    });
    await waitFor(() =>
      expect(
        emailRendererDocument(reopenedEarlierDocument)
          ?.querySelector("img")
          ?.hasAttribute("src"),
      ).toBe(false),
    );
    expect(
      screen.getByRole("button", { name: "Load remote content" }),
    ).toBeVisible();
  });

  it("discards an old mailbox epoch response before it can replace a reused message", async () => {
    let resolveOld: ((content: MessageContent) => void) | undefined;
    let resolveNew: ((content: MessageContent) => void) | undefined;
    vi.mocked(api.content).mockImplementation(
      async (_messageId, expectedUidValidity) =>
        new Promise<MessageContent>((resolve) => {
          if (expectedUidValidity === 77) resolveOld = resolve;
          if (expectedUidValidity === 78) resolveNew = resolve;
        }),
    );
    const saveAttachment = vi
      .spyOn(api, "saveAttachment")
      .mockResolvedValue("/Users/alex/Downloads/new.pdf");
    const oldMessage = {
      ...message,
      mailbox_uid_validity: 77,
      subject: "Old mailbox headers",
      has_attachments: true,
    };
    const newMessage = {
      ...oldMessage,
      mailbox_uid_validity: 78,
      subject: "Replacement mailbox headers",
    };
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={oldMessage} />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(api.content).toHaveBeenCalledWith(oldMessage.id, 77),
    );
    rerender(
      <MantineProvider>
        <Reader {...props} message={newMessage} />
      </MantineProvider>,
    );

    expect(screen.getByText("Replacement mailbox headers")).toBeVisible();
    expect(screen.queryByText("Old mailbox headers")).toBeNull();
    await waitFor(() =>
      expect(api.content).toHaveBeenCalledWith(newMessage.id, 78),
    );

    await act(async () => {
      resolveOld?.({
        body_text: "Old mailbox body",
        attachments: [
          {
            id: "old-attachment",
            message_id: oldMessage.id,
            filename: "old.pdf",
            mime_type: "application/pdf",
            size_bytes: 100,
            is_inline: false,
            presentation: "downloadable",
            is_potentially_unsafe: false,
          },
        ],
      });
    });
    expect(screen.queryByText("Old mailbox body")).toBeNull();
    expect(screen.queryByText("old.pdf")).toBeNull();

    await act(async () => {
      resolveNew?.({
        body_text: "Replacement mailbox body",
        attachments: [
          {
            id: "new-attachment",
            message_id: newMessage.id,
            filename: "new.pdf",
            mime_type: "application/pdf",
            size_bytes: 100,
            is_inline: false,
            presentation: "downloadable",
            is_potentially_unsafe: false,
          },
        ],
      });
    });
    expect(await screen.findByText("Replacement mailbox body")).toBeVisible();
    expect(screen.queryByText("Old mailbox body")).toBeNull();
    fireEvent.click(
      screen.getByRole("button", { name: "Save new.pdf to Downloads" }),
    );
    await waitFor(() =>
      expect(saveAttachment).toHaveBeenCalledWith(
        newMessage.id,
        "new-attachment",
        78,
      ),
    );
  });

  it("disables unsubscribe while the request is in progress", () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader
          {...props}
          unsubscribeLoading
          message={{ ...message, unsubscribe_kind: "one_click" }}
        />
      </MantineProvider>,
    );
    expect(screen.getByRole("button", { name: "Unsubscribe" })).toBeDisabled();
  });

  it("marks messages sent by the active account", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          accountEmail="me@example.com"
          message={{
            ...message,
            mailbox: "Sent",
            from_address: "me@example.com",
          }}
        />
      </MantineProvider>,
    );
    expect(screen.getByText("Sent by you")).toBeVisible();
  });

  it("loads an expanded complete message through content without foreground hydration", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(await screen.findByText("Tere maailm")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(1);
    expect(api.content).toHaveBeenCalledWith(message.id, null);
    expect(api.hydrateMessage).not.toHaveBeenCalled();
  });

  it("loads header-first arrivals through content without foreground hydration", async () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, body_text: "", content_state: "headers_only" }}
        />
      </MantineProvider>,
    );
    expect(screen.getByRole("status")).toHaveTextContent(
      "Loading the full message",
    );
    expect(await screen.findByText("Tere maailm")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(1);
    expect(api.content).toHaveBeenCalledWith(message.id, null);
    expect(api.hydrateMessage).not.toHaveBeenCalled();
  });

  it("retries content without foreground hydration after a load failure", async () => {
    vi.mocked(api.content)
      .mockRejectedValueOnce(new MessageContentError("transient"))
      .mockResolvedValueOnce({ body_text: "Recovered body", attachments: [] });
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, body_text: "", content_state: "headers_only" }}
        />
      </MantineProvider>,
    );

    expect(
      await screen.findByText(
        "This message could not be fetched from the mail server.",
      ),
    ).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(1);
    expect(api.hydrateMessage).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("Recovered body")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(2);
    expect(api.content).toHaveBeenLastCalledWith(message.id, null);
    expect(api.hydrateMessage).not.toHaveBeenCalled();
  });

  it("offers forwarding from the message actions menu", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Forward" }));
    expect(props.onForward).toHaveBeenCalledOnce();
    expect(props.onForward).toHaveBeenCalledWith(message);
  });

  it("offers send again only for sent messages", async () => {
    const sentMessage = {
      ...message,
      mailbox: "Sent",
      from_address: "me@example.com",
      to_addresses: "recipient@example.com",
    };
    render(
      <MantineProvider>
        <Reader
          {...props}
          accountEmail="me@example.com"
          message={sentMessage}
        />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Send again" }),
    );
    expect(props.onSendAgain).toHaveBeenCalledWith(sentMessage);

    cleanup();
    render(
      <MantineProvider>
        <Reader {...props} accountEmail="me@example.com" message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    expect(
      screen.queryByRole("menuitem", { name: "Send again" }),
    ).not.toBeInTheDocument();

    cleanup();
    render(
      <MantineProvider>
        <Reader
          {...props}
          accountEmail="me@example.com"
          message={{
            ...sentMessage,
            id: "outbox-message",
            mailbox: "Outbox",
          }}
        />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    expect(
      screen.queryByRole("menuitem", { name: "Send again" }),
    ).not.toBeInTheDocument();
  });

  it("exports the selected message once and reports the saved path", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Export message (.eml)" }),
    );

    await waitFor(() =>
      expect(api.exportMessage).toHaveBeenCalledWith("message-1", null),
    );
    expect(
      await screen.findByText(
        "Message exported to /Users/alex/Downloads/weekly-notes.eml",
      ),
    ).toBeVisible();
  });

  it("prevents a second export while the current export is pending", async () => {
    let finishExport: ((path: string) => void) | undefined;
    vi.mocked(api.exportMessage).mockImplementationOnce(
      () =>
        new Promise<string>((resolve) => {
          finishExport = resolve;
        }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    const pendingExport = await screen.findByRole("menuitem", {
      name: "Export message (.eml)",
    });
    fireEvent.click(pendingExport);
    await waitFor(() => expect(pendingExport).toBeDisabled());
    expect(pendingExport).toBeDisabled();
    fireEvent.click(pendingExport);
    expect(api.exportMessage).toHaveBeenCalledOnce();

    finishExport?.("/Users/alex/Downloads/weekly-notes.eml");
    expect(
      await screen.findByText(
        "Message exported to /Users/alex/Downloads/weekly-notes.eml",
      ),
    ).toBeVisible();
  });

  it("reports an export failure without claiming the message was saved", async () => {
    vi.mocked(api.exportMessage).mockRejectedValueOnce(new Error("cancelled"));
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Export message (.eml)" }),
    );

    expect(
      await screen.findByText("Could not export this message."),
    ).toBeVisible();
    expect(screen.queryByText(/Message exported to/)).not.toBeInTheDocument();
  });

  it("runs archive from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    expect(props.onArchive).toHaveBeenCalledOnce();
  });

  it("opens reply from the latest message header action", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} messages={[message]} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Quick reply" }));
    expect(props.onReply).toHaveBeenCalledOnce();
    expect(props.onReply).toHaveBeenCalledWith(message);
  });

  it("routes actions from an expanded older message to that message", async () => {
    const older = {
      ...message,
      id: "message-older",
      from_name: "Older sender",
    };
    const latest = {
      ...message,
      id: "message-latest",
      from_name: "Latest sender",
    };
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Expanded body",
      attachments: [],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={older} messages={[older, latest]} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Expand message from Older sender" }),
    );
    await screen.findByText("Expanded body");
    fireEvent.click(screen.getByRole("button", { name: "Quick reply" }));
    fireEvent.click(screen.getByRole("button", { name: "Reply all" }));
    fireEvent.click(screen.getByRole("button", { name: "Forward" }));

    expect(props.onReply).toHaveBeenCalledWith(older);
    expect(props.onReplyAll).toHaveBeenCalledWith(older);
    expect(props.onForward).toHaveBeenCalledWith(older);
  });

  it("keeps the subject as one heading and compacts it after its sentinel leaves the reader", async () => {
    const observers: Array<{
      callback: IntersectionObserverCallback;
      observe: ReturnType<typeof vi.fn>;
      disconnect: ReturnType<typeof vi.fn>;
    }> = [];
    class MockIntersectionObserver {
      callback: IntersectionObserverCallback;
      observe = vi.fn();
      disconnect = vi.fn();
      constructor(callback: IntersectionObserverCallback) {
        this.callback = callback;
        observers.push(this);
      }
    }
    vi.stubGlobal("IntersectionObserver", MockIntersectionObserver);
    try {
      const longSubject =
        "A subject that stays available while reading a long conversation";
      const { rerender } = render(
        <MantineProvider>
          <Reader {...props} message={{ ...message, subject: longSubject }} />
        </MantineProvider>,
      );
      const heading = screen.getByRole("heading", {
        level: 1,
        name: longSubject,
      });
      expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
      expect(heading).not.toHaveAttribute("data-compact");
      expect(observers).toHaveLength(1);

      act(() => {
        observers[0].callback(
          [{ isIntersecting: false } as IntersectionObserverEntry],
          {} as IntersectionObserver,
        );
      });
      expect(heading).toHaveAttribute("data-compact", "true");
      expect(heading).toHaveAttribute("title", longSubject);

      rerender(
        <MantineProvider>
          <Reader
            {...props}
            message={{
              ...message,
              id: "message-next",
              thread_id: "thread-next",
              subject: "Next conversation",
            }}
          />
        </MantineProvider>,
      );
      await waitFor(() =>
        expect(screen.getByRole("heading", { level: 1 })).not.toHaveAttribute(
          "data-compact",
        ),
      );
      expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("does not show a stale attachment panel for embedded signature artwork", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Signed message",
      attachments: [
        {
          id: "signature-logo",
          message_id: message.id,
          filename: "image001.png",
          mime_type: "image/png",
          size_bytes: 7_000,
          is_inline: true,
          presentation: "embedded",
          is_potentially_unsafe: false,
        },
      ],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    await screen.findByText("Signed message");
    await waitFor(() =>
      expect(
        screen.queryByRole("region", { name: "Attachments" }),
      ).not.toBeInTheDocument(),
    );
    expect(screen.queryByRole("button", { name: "Save all" })).toBeNull();
    expect(screen.queryByLabelText("Has attachments")).toBeNull();
  });

  it("renders provider-signature-inline shared message content", async () => {
    expect(highRiskContract.realisticFixtureIds.providerSignature).toBe(
      "provider-signature-inline",
    );
    vi.mocked(api.content).mockResolvedValue(
      highRiskContract.messageContent.providerSignature as MessageContent,
    );
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    expect(await screen.findByText("claim-documents.pdf")).toBeVisible();
    expect(screen.queryByText("image001.png")).toBeNull();
    const renderedMessage = await screen.findByRole("document", {
      name: "Weekly notes",
    });
    const emailRoot = emailRendererDocument(renderedMessage);
    expect(emailRoot?.querySelector("img")?.getAttribute("src")).toBe(
      "data:image/png;base64,iVBORw0KGgo=",
    );
    expect(emailRoot?.textContent).toContain(
      "Fictional confidential-message notice.",
    );
  });

  it("lists returned downloadable attachments and saves all only when there are multiple", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Files included",
      attachments: [
        {
          id: "embedded-logo",
          message_id: message.id,
          filename: "image001.png",
          mime_type: "image/png",
          size_bytes: 7_000,
          is_inline: true,
          presentation: "embedded",
          is_potentially_unsafe: false,
        },
        {
          id: "invoice",
          message_id: message.id,
          filename: "invoice.pdf",
          mime_type: "application/pdf",
          size_bytes: 12_000,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
        {
          id: "attached-inline",
          message_id: message.id,
          filename: "diagram.png",
          mime_type: "image/png",
          size_bytes: 8_000,
          is_inline: true,
          presentation: "both",
          is_potentially_unsafe: false,
        },
      ],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    expect(await screen.findByText("invoice.pdf")).toBeVisible();
    expect(screen.getByText("diagram.png")).toBeVisible();
    expect(screen.queryByText("image001.png")).toBeNull();
    expect(screen.getByRole("button", { name: "Save all" })).toBeVisible();
  });

  it("does not offer Save all for one downloadable attachment", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "One file included",
      attachments: [
        {
          id: "invoice",
          message_id: message.id,
          filename: "invoice.pdf",
          mime_type: "application/pdf",
          size_bytes: 12_000,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
      ],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    expect(await screen.findByText("invoice.pdf")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Save all" })).toBeNull();
  });

  it("shows a numeric attachment size only after decoded bytes are available", async () => {
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Attachment sizes",
      attachments: [
        {
          id: "downloaded",
          message_id: message.id,
          filename: "downloaded.pdf",
          mime_type: "application/pdf",
          size_bytes: 12_000,
          size_kind: "decoded",
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
        {
          id: "metadata-only",
          message_id: message.id,
          filename: "metadata-only.pdf",
          mime_type: "application/pdf",
          // This is BODYSTRUCTURE's encoded transport length, not a file size.
          size_bytes: 16_384,
          size_kind: "encoded",
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
        {
          id: "legacy",
          message_id: message.id,
          filename: "legacy.pdf",
          mime_type: "application/pdf",
          size_bytes: 4_096,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
      ],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    expect(await screen.findByText("downloaded.pdf")).toBeVisible();
    expect(screen.getByText(/12 KB · application\/pdf/)).toBeVisible();
    expect(
      screen.getAllByText(/Size unavailable · application\/pdf/),
    ).toHaveLength(2);
    expect(screen.queryByText(/16 KB/)).toBeNull();
    expect(screen.queryByText(/4 KB/)).toBeNull();
    const save = vi
      .spyOn(api, "saveAttachment")
      .mockResolvedValue("/fictional/metadata-only.pdf");
    fireEvent.click(
      screen.getByRole("button", {
        name: "Save metadata-only.pdf to Downloads",
      }),
    );
    await waitFor(() => expect(save).toHaveBeenCalled());
    // Saving returns a path, not refreshed size metadata. Do not promise that
    // the encoded count has become a known file size after this action.
    expect(
      screen.getAllByText(/Size unavailable · application\/pdf/),
    ).toHaveLength(2);
  });

  it("opens a conversation on its final message and fetches earlier messages only when expanded", async () => {
    const earliest = {
      ...message,
      id: "message-0",
      uid: 0,
      from_name: "Earlier Sender",
      received_at: "2026-07-19T08:00:00Z",
      snippet: "Earlier preview",
      has_attachments: true,
    };
    const middle = {
      ...message,
      id: "message-middle",
      uid: 2,
      from_name: "Middle Sender",
      received_at: "2026-07-19T09:00:00Z",
      snippet: "Middle preview",
    };
    const latest = {
      ...message,
      id: "message-latest",
      uid: 3,
      from_name: "Latest Sender",
      received_at: "2026-07-19T11:00:00Z",
      snippet: "Latest preview",
      unsubscribe_kind: "one_click" as const,
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text:
        id === earliest.id
          ? "Earlier full body"
          : id === middle.id
            ? "Middle full body"
            : "Latest full body",
      attachments: [],
    }));

    const { rerender } = render(
      <MantineProvider>
        <Reader
          {...props}
          message={earliest}
          messages={[earliest, middle, latest]}
        />
      </MantineProvider>,
    );

    const earliestDisclosure = screen.getByRole("button", {
      name: "Expand message from Earlier Sender",
    });
    expect(earliestDisclosure).toHaveAttribute("aria-expanded", "false");
    expect(earliestDisclosure).toHaveAccessibleDescription(
      /Earlier preview.*19 Jul at.*Has attachments/,
    );
    expect(screen.getByText("Earlier preview")).toBeVisible();
    expect(screen.getByText("Middle preview")).toBeVisible();
    expect(screen.getByLabelText("Has attachments")).toBeVisible();
    expect(
      screen.getByRole("button", {
        name: "Collapse message from Latest Sender",
      }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(await screen.findByText("Latest full body")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(1);
    expect(api.content).toHaveBeenCalledWith(latest.id, null);

    fireEvent.click(earliestDisclosure);
    expect(await screen.findByText("Earlier full body")).toBeVisible();
    expect(screen.queryByText("Latest full body")).not.toBeInTheDocument();
    expect(api.content).toHaveBeenCalledTimes(2);
    expect(api.content).toHaveBeenCalledWith(earliest.id, null);
    expect(api.content).not.toHaveBeenCalledWith(middle.id, null);

    fireEvent.click(
      screen.getByRole("button", {
        name: "Collapse message from Earlier Sender",
      }),
    );
    expect(screen.queryByText("Earlier full body")).not.toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Earlier Sender",
      }),
    );
    expect(await screen.findByText("Earlier full body")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(2);

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={earliest}
          messages={[
            { ...earliest, snippet: "Updated earlier preview" },
            middle,
            latest,
          ]}
        />
      </MantineProvider>,
    );
    expect(
      screen.getByRole("button", {
        name: "Collapse message from Earlier Sender",
      }),
    ).toHaveAttribute("aria-expanded", "true");

    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Latest Sender",
      }),
    );
    expect(await screen.findByText("Latest full body")).toBeVisible();
    expect(screen.getByRole("button", { name: "Quick reply" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Unsubscribe" })).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    expect(props.onUnsubscribe).toHaveBeenCalledWith(latest);
    expect(api.content).toHaveBeenCalledTimes(2);
    fireEvent.click(
      screen.getByRole("button", {
        name: "Collapse message from Latest Sender",
      }),
    );
    expect(screen.queryByText("Latest full body")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", {
        name: "Expand message from Latest Sender",
      }),
    ).toHaveAttribute("aria-expanded", "false");

    const nextEarlier = {
      ...earliest,
      id: "message-next-earlier",
      thread_id: "thread-next",
      from_name: "Next earlier sender",
    };
    const nextLatest = {
      ...latest,
      id: "message-next-latest",
      thread_id: "thread-next",
      from_name: "Next latest sender",
    };
    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={nextEarlier}
          messages={[nextEarlier, nextLatest]}
        />
      </MantineProvider>,
    );
    expect(
      screen.getByRole("button", {
        name: "Expand message from Next earlier sender",
      }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(await screen.findByText("Latest full body")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(3);
    expect(api.content).toHaveBeenLastCalledWith(nextLatest.id, null);
  });

  it("opens a dedicated reader on the explicitly focused thread message", async () => {
    const earlier = {
      ...message,
      id: "focused-earlier",
      from_name: "Earlier Sender",
      received_at: "2026-07-19T09:00:00Z",
    };
    const latest = {
      ...message,
      id: "unfocused-latest",
      uid: 2,
      from_name: "Latest Sender",
      received_at: "2026-07-19T11:00:00Z",
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text: id === earlier.id ? "Focused body" : "Latest body",
      attachments: [],
    }));

    render(
      <MantineProvider>
        <Reader
          {...props}
          message={earlier}
          messages={[earlier, latest]}
          focusedMessageId={earlier.id}
        />
      </MantineProvider>,
    );

    expect(
      screen.getByRole("button", {
        name: "Collapse message from Earlier Sender",
      }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(await screen.findByText("Focused body")).toBeVisible();
    expect(api.content).not.toHaveBeenCalledWith(latest.id, null);
  });

  it("keeps the selected expanded message open when a newer message arrives", async () => {
    const earlier = {
      ...message,
      id: "message-earlier",
      from_name: "Earlier Sender",
    };
    const latest = {
      ...message,
      id: "message-latest",
      from_name: "Latest Sender",
    };
    const newer = {
      ...message,
      id: "message-newer",
      from_name: "Newer Sender",
    };
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Full message body",
      attachments: [],
    });

    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={earlier} messages={[earlier, latest]} />
      </MantineProvider>,
    );
    await screen.findByText("Full message body");
    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Earlier Sender",
      }),
    );
    await screen.findByText("Full message body");

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={earlier}
          messages={[earlier, latest, newer]}
        />
      </MantineProvider>,
    );

    expect(
      screen.getByRole("button", {
        name: "Collapse message from Earlier Sender",
      }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(
      screen.getByRole("button", {
        name: "Expand message from Newer Sender",
      }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(api.content).toHaveBeenCalledTimes(2);
    expect(api.content).not.toHaveBeenCalledWith(newer.id, null);
  });

  it("only collapses threaded messages through their collapse control", async () => {
    const earlier = {
      ...message,
      id: "message-earlier",
      from_name: "Earlier Sender",
    };
    const latest = {
      ...message,
      id: "message-latest",
      from_name: "Latest Sender",
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text: id === latest.id ? "Latest full body" : "Earlier full body",
      attachments: [],
    }));
    render(
      <MantineProvider>
        <Reader {...props} message={earlier} messages={[earlier, latest]} />
      </MantineProvider>,
    );

    expect(await screen.findByText("Latest full body")).toBeVisible();
    fireEvent.click(screen.getByText("Latest Sender"));
    expect(screen.getByText("Latest full body")).toBeVisible();

    fireEvent.click(
      screen.getByRole("button", {
        name: "Collapse message from Latest Sender",
      }),
    );
    expect(screen.queryByText("Latest full body")).not.toBeInTheDocument();

    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Latest Sender",
      }),
    );
    expect(await screen.findByText("Latest full body")).toBeVisible();
  });

  it("does not offer collapse for a single-message conversation", async () => {
    const namedMessage = { ...message, from_name: "Only Sender" };
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Only message body",
      attachments: [],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={namedMessage} />
      </MantineProvider>,
    );

    expect(await screen.findByText("Only message body")).toBeVisible();
    expect(
      screen.queryByRole("button", {
        name: "Collapse message from Only Sender",
      }),
    ).toBeNull();
    fireEvent.click(screen.getByText("Only Sender"));
    expect(screen.getByText("Only message body")).toBeVisible();
  });

  it("resets expanded state for the same thread ID in another account", async () => {
    const firstAccountEarlier = {
      ...message,
      id: "account-one-earlier",
      account_id: "account-one",
      from_name: "First account earlier",
    };
    const firstAccountLatest = {
      ...message,
      id: "account-one-latest",
      account_id: "account-one",
      from_name: "First account latest",
    };
    const secondAccountEarlier = {
      ...message,
      id: "account-two-earlier",
      account_id: "account-two",
      from_name: "Second account earlier",
    };
    const secondAccountLatest = {
      ...message,
      id: "account-two-latest",
      account_id: "account-two",
      from_name: "Second account latest",
    };
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Account-scoped body",
      attachments: [],
    });

    const { rerender } = render(
      <MantineProvider>
        <Reader
          {...props}
          message={firstAccountLatest}
          messages={[firstAccountEarlier, firstAccountLatest]}
        />
      </MantineProvider>,
    );
    await screen.findByText("Account-scoped body");
    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from First account earlier",
      }),
    );

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={secondAccountLatest}
          messages={[secondAccountEarlier, secondAccountLatest]}
        />
      </MantineProvider>,
    );

    expect(
      screen.getByRole("button", {
        name: "Collapse message from Second account latest",
      }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(
      screen.getByRole("button", {
        name: "Expand message from Second account earlier",
      }),
    ).toHaveAttribute("aria-expanded", "false");
    await waitFor(() =>
      expect(api.content).toHaveBeenCalledWith(secondAccountLatest.id, null),
    );
  });

  it("expands complete recipient details with interactive address controls", () => {
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={{
            ...message,
            from_name: "Mail Sender",
            cc_addresses: '"Doe, Jane" <jane@example.com>',
            bcc_addresses: "blind@example.com",
            reply_to_addresses: "replies@example.com",
          }}
        />
      </MantineProvider>,
    );

    const disclosure = screen.getByRole("button", {
      name: "Show full recipient details",
    });
    expect(disclosure).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(disclosure);
    expect(
      screen.getByRole("button", { name: "Hide full recipient details" }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(
      screen.getAllByRole("button", { name: "Email list@example.com" }),
    ).toHaveLength(2);
    expect(
      screen.getAllByRole("button", { name: "Email me@example.com" }),
    ).toHaveLength(2);
    expect(
      screen.getByRole("button", { name: "Email jane@example.com" }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Email blind@example.com" }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Email replies@example.com" }),
    ).toBeVisible();
  });

  it("preserves unsupported raw mailbox headers without making them interactive", () => {
    const rawRecipient = '"Jane Doe"@example.com, valid@example.com';
    render(
      <MantineProvider>
        <Reader
          {...props}
          message={{
            ...message,
            from_name: "Provider fallback",
            from_address: "malformed provider fallback",
            to_addresses: rawRecipient,
          }}
        />
      </MantineProvider>,
    );

    expect(screen.getByText("malformed provider fallback")).toBeVisible();
    expect(screen.getByText(rawRecipient)).toBeVisible();
    expect(
      screen.queryByRole("button", {
        name: "Email malformed provider fallback",
      }),
    ).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Email valid@example.com" }),
    ).toBeNull();
  });

  it("opens a native compose target from an address without collapsing the message", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Email list@example.com" }),
    );

    expect(props.onComposeTo).toHaveBeenCalledWith(message, "list@example.com");
    expect(
      screen.queryByRole("button", {
        name: "Collapse message from list@example.com",
      }),
    ).toBeNull();
  });

  it("requests the native context menu for the right-clicked address", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    const address = screen.getByRole("button", {
      name: "Email list@example.com",
    });
    const contextEvent = createEvent.contextMenu(address, {
      clientX: 140,
      clientY: 90,
    });
    fireEvent(address, contextEvent);

    expect(contextEvent.defaultPrevented).toBe(true);
    expect(props.onAddressContextMenu).toHaveBeenCalledWith(
      message,
      "list@example.com",
    );
    expect(
      screen.queryByRole("button", {
        name: "Collapse message from list@example.com",
      }),
    ).toBeNull();
    expect(screen.queryByRole("menuitem")).toBeNull();
  });

  it("preserves a drag selection instead of opening a compose window", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    const address = screen.getByRole("button", {
      name: "Email list@example.com",
    });
    const selection = window.getSelection()!;
    const range = document.createRange();
    range.selectNodeContents(address);
    selection.removeAllRanges();
    selection.addRange(range);

    fireEvent.click(address);

    expect(selection.toString()).toBe("list@example.com");
    expect(props.onComposeTo).not.toHaveBeenCalled();
    selection.removeAllRanges();
  });

  it("does not collapse the header when a selection extends beyond an address", () => {
    const namedMessage = { ...message, from_name: "Mail Sender" };
    render(
      <MantineProvider>
        <Reader {...props} message={namedMessage} />
      </MantineProvider>,
    );

    const senderName = screen.getByText("Mail Sender");
    const address = screen.getByRole("button", {
      name: "Email list@example.com",
    });
    const selection = window.getSelection()!;
    const range = document.createRange();
    range.setStart(senderName.firstChild!, 0);
    range.setEnd(address.firstChild!, "list@example.com".length);
    selection.removeAllRanges();
    selection.addRange(range);

    fireEvent.click(senderName);

    expect(selection.toString()).toBe("Mail Senderlist@example.com");
    expect(
      screen.queryByRole("button", {
        name: "Collapse message from Mail Sender",
      }),
    ).toBeNull();
    selection.removeAllRanges();
  });

  it("offers Reply All at the bottom and in the message menu", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Reply all" }));
    expect(props.onReplyAll).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Reply all" }));
    expect(props.onReplyAll).toHaveBeenCalledTimes(2);
  });

  it("collapses strong quoted plain-text history", async () => {
    vi.mocked(api.content).mockResolvedValueOnce({
      body_text: "Fresh answer\n\nOn Monday, Pat wrote:\n> Old\n> Message",
      attachments: [],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(
      await screen.findByText("Fresh answer", { exact: false }),
    ).toBeVisible();
    const details = screen.getByText("Show history").closest("details");
    expect(details).not.toHaveAttribute("open");
    fireEvent.click(screen.getByText("Show history"));
    expect(details).toHaveAttribute("open");
    expect(screen.getByText(/On Monday, Pat wrote/)).toBeVisible();
  });

  it("collapses Freshdesk history in the latest message of a thread", async () => {
    const older = {
      ...message,
      id: "message-older",
      received_at: "2026-07-19T09:00:00Z",
      subject: "Earlier reply",
    };
    const latest = {
      ...message,
      id: "message-latest",
      received_at: "2026-07-19T10:00:00Z",
      subject: "Latest Freshdesk reply",
    };
    vi.mocked(api.content).mockImplementation(async (messageId) =>
      messageId === latest.id
        ? {
            body_text: "Latest fallback",
            body_html: freshdeskReplySection,
            attachments: [],
          }
        : {
            body_text: "Earlier message remains independently visible.",
            attachments: [],
          },
    );

    render(
      <MantineProvider>
        <Reader {...props} message={latest} messages={[older, latest]} />
      </MantineProvider>,
    );

    const latestMessage = await screen.findByRole("document", {
      name: "Latest Freshdesk reply",
    });
    const historyHost = latestMessage.querySelector<HTMLElement>(
      '[data-dakia-email-surface="history"]',
    );
    const currentDocument = await waitFor(() => {
      const document = emailRendererDocument(latestMessage);
      expect(document).not.toBeNull();
      return document!;
    });
    const historyDocument = await waitFor(() => {
      const document = emailRendererDocument(historyHost);
      expect(document).not.toBeNull();
      return document!;
    });
    const historyButton = screen.getByRole("button", { name: "Show history" });

    expect(
      screen.getAllByRole("button", { name: "Show history" }),
    ).toHaveLength(1);
    expect(historyButton).toHaveAttribute("aria-expanded", "false");
    expect(currentDocument?.textContent).toContain("Good afternoon, Customer");
    expect(currentDocument?.textContent).toContain(
      "Thank you for letting us know.",
    );
    expect(currentDocument?.textContent).not.toContain(
      "Earlier support reply.",
    );
    expect(historyHost?.parentElement).toHaveAttribute("aria-hidden", "true");

    for (let cycle = 0; cycle < 2; cycle += 1) {
      fireEvent.click(historyButton);
      expect(historyButton).toHaveAccessibleName("Hide history");
      expect(historyButton).toHaveAttribute("aria-expanded", "true");
      expect(historyHost?.parentElement).toHaveAttribute(
        "aria-hidden",
        "false",
      );
      expect(historyDocument?.textContent).toContain("Earlier support reply.");
      expect(currentDocument?.textContent).toContain(
        "Good afternoon, Customer",
      );

      fireEvent.click(historyButton);
      expect(historyButton).toHaveAccessibleName("Show history");
      expect(historyButton).toHaveAttribute("aria-expanded", "false");
      expect(historyHost?.parentElement).toHaveAttribute("aria-hidden", "true");
      expect(currentDocument?.textContent).toContain(
        "Thank you for letting us know.",
      );
    }
    expect(api.content).toHaveBeenCalledWith("message-latest", null);
    expect(api.content).not.toHaveBeenCalledWith("message-older", null);
  });

  it("keeps a CID-resolved image in the message body while listing named attachments separately", async () => {
    vi.mocked(api.content).mockResolvedValueOnce({
      body_text: "Your July statement is ready.",
      body_html:
        '<p>Your July statement is ready.</p><img src="data:image/png;base64,iVBORw0KGgo=" alt="Statement logo">',
      attachments: [
        {
          id: "cid-logo",
          message_id: message.id,
          filename: "statement-logo.png",
          mime_type: "image/png",
          size_bytes: 42,
          is_inline: true,
          presentation: "both",
          is_potentially_unsafe: false,
        },
        {
          id: "statement-pdf",
          message_id: message.id,
          filename: "July-statement.pdf",
          mime_type: "application/pdf",
          size_bytes: 1_024,
          is_inline: false,
          presentation: "downloadable",
          is_potentially_unsafe: false,
        },
      ],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, has_attachments: true }} />
      </MantineProvider>,
    );

    const renderedMessage = await screen.findByRole("document", {
      name: "Weekly notes",
    });
    const emailRoot = emailRendererDocument(renderedMessage);
    expect(emailRoot?.textContent).toContain("Your July statement is ready.");
    expect(emailRoot?.querySelector("img")?.getAttribute("src")).toBe(
      "data:image/png;base64,iVBORw0KGgo=",
    );
    expect(emailRoot?.querySelector("img")?.getAttribute("alt")).toBe(
      "Statement logo",
    );

    const attachmentPanel = screen.getByRole("region", {
      name: "Attachments",
    });
    expect(attachmentPanel).toHaveTextContent("statement-logo.png");
    expect(attachmentPanel).toHaveTextContent("July-statement.pdf");
    expect(
      screen.getByRole("button", {
        name: "Save July-statement.pdf to Downloads",
      }),
    ).toBeEnabled();
  });

  it("does not offer retry when a message exceeds the MIME resource limit", async () => {
    vi.mocked(api.content).mockRejectedValueOnce(
      new MessageContentError("resource_limit"),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(
      await screen.findByText(
        "This message is too large or complex for Dakia to open.",
      ),
    ).toBeVisible();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(api.content).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["malformed", "This message is malformed and cannot be opened."],
    ["undecodable", "The contents of this message cannot be decoded."],
    ["unsupported", "This message uses an unsupported format."],
  ] as const)(
    "shows the non-retryable %s content outcome",
    async (kind, expectedCopy) => {
      vi.mocked(api.content).mockRejectedValueOnce(
        new MessageContentError(kind),
      );
      render(
        <MantineProvider>
          <Reader {...props} message={message} />
        </MantineProvider>,
      );

      expect(await screen.findByText(expectedCopy)).toBeVisible();
      expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
      expect(api.content).toHaveBeenCalledTimes(1);
    },
  );

  it("runs delete from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(props.onTrash).toHaveBeenCalledOnce();
  });

  it("offers delete from the message actions menu", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Delete" }));
    expect(props.onTrash).toHaveBeenCalledOnce();
  });

  it("offers permanent deletion only in a message menu and confirms the exact message once", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    expect(
      screen.queryByRole("button", { name: "Permanently delete" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledWith(
        "Permanently delete this message?",
        "Dakia will request deletion of this message without moving it to Trash. It cannot be undone in Dakia; your email provider controls the final IMAP disposition.",
        "Permanently delete",
      ),
    );
    expect(translationMocks.confirm).toHaveBeenCalledOnce();
    expect(props.onPermanentDelete).toHaveBeenCalledWith(message);
  });

  it("does not delete when the permanent-delete confirmation is cancelled", async () => {
    translationMocks.confirm.mockResolvedValueOnce(false);
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
  });

  it("reports a failed permanent-delete confirmation without mutating", async () => {
    translationMocks.confirm.mockRejectedValueOnce(new Error("dialog closed"));
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });

    await waitFor(() =>
      expect(translationMocks.show).toHaveBeenCalledWith(
        "Could not permanently delete this message",
        "Error: dialog closed",
        "error",
      ),
    );
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
  });

  it("permanently deletes the expanded older message instead of the latest message", async () => {
    const older = {
      ...message,
      id: "permanent-delete-older",
      uid: 40,
      from_name: "Older sender",
    };
    const latest = {
      ...message,
      id: "permanent-delete-latest",
      uid: 41,
      from_name: "Latest sender",
    };
    vi.mocked(api.content).mockResolvedValue({
      body_text: "Expanded older body",
      attachments: [],
    });
    render(
      <MantineProvider>
        <Reader {...props} message={latest} messages={[older, latest]} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Expand message from Older sender" }),
    );
    await screen.findByText("Expanded older body");
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    await waitFor(() =>
      expect(props.onPermanentDelete).toHaveBeenCalledWith(older),
    );
    expect(props.onPermanentDelete).not.toHaveBeenCalledWith(latest);
  });

  it("uses Shift+Delete for the exact expanded message", async () => {
    const older = {
      ...message,
      id: "shortcut-older",
      uid: 40,
      from_name: "Older sender",
    };
    const latest = {
      ...message,
      id: "shortcut-latest",
      uid: 41,
      from_name: "Latest sender",
    };
    render(
      <MantineProvider>
        <Reader {...props} message={latest} messages={[older, latest]} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Expand message from Older sender" }),
    );

    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });

    await waitFor(() =>
      expect(props.onPermanentDelete).toHaveBeenCalledWith(older),
    );
    expect(props.onPermanentDelete).not.toHaveBeenCalledWith(latest);
    expect(props.onTrash).not.toHaveBeenCalled();
  });

  it("does not treat Delete or Backspace without Shift as permanent deletion", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.keyDown(document.documentElement, { key: "Delete" });
    fireEvent.keyDown(document.documentElement, { key: "Backspace" });

    expect(translationMocks.confirm).not.toHaveBeenCalled();
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
  });

  it("accepts macOS Shift+Backspace as Shift+Delete", async () => {
    const platform = vi
      .spyOn(window.navigator, "platform", "get")
      .mockReturnValue("MacIntel");
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.keyDown(document.documentElement, {
      key: "Backspace",
      shiftKey: true,
    });

    await waitFor(() =>
      expect(props.onPermanentDelete).toHaveBeenCalledWith(message),
    );
    platform.mockRestore();
  });

  it("does not use Shift+Backspace as permanent delete off macOS", () => {
    const platform = vi
      .spyOn(window.navigator, "platform", "get")
      .mockReturnValue("Win32");
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    const event = new KeyboardEvent("keydown", {
      key: "Backspace",
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    document.documentElement.dispatchEvent(event);

    expect(translationMocks.confirm).not.toHaveBeenCalled();
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
    expect(event.defaultPrevented).toBe(false);
    platform.mockRestore();
  });

  it("ignores Shift+Delete in editable controls and while actions are disabled", () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    const input = document.createElement("input");
    document.body.append(input);
    fireEvent.keyDown(input, { key: "Delete", shiftKey: true });
    const editor = document.createElement("div");
    Object.defineProperty(editor, "isContentEditable", { value: true });
    document.body.append(editor);
    fireEvent.keyDown(editor, { key: "Delete", shiftKey: true });

    rerender(
      <MantineProvider>
        <Reader {...props} message={message} actionsDisabled />
      </MantineProvider>,
    );
    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });
    input.remove();
    editor.remove();

    expect(translationMocks.confirm).not.toHaveBeenCalled();
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
  });

  it("coalesces repeated Shift+Delete events into one confirmation", async () => {
    let resolveConfirmation: ((confirmed: boolean) => void) | undefined;
    translationMocks.confirm.mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          resolveConfirmation = resolve;
        }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });
    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
      repeat: true,
    });
    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });

    expect(translationMocks.confirm).toHaveBeenCalledOnce();
    await act(async () => resolveConfirmation!(false));
    expect(props.onPermanentDelete).not.toHaveBeenCalled();
  });

  it("keeps Shift+Delete locked until the deletion operation finishes", async () => {
    let finishDeletion: (() => void) | undefined;
    props.onPermanentDelete.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          finishDeletion = resolve;
        }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });
    await waitFor(() => expect(props.onPermanentDelete).toHaveBeenCalledOnce());
    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });

    expect(translationMocks.confirm).toHaveBeenCalledOnce();
    expect(props.onPermanentDelete).toHaveBeenCalledOnce();
    await act(async () => finishDeletion!());
  });

  it("switches the toolbar action to Not spam for spam messages", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={{ ...message, mailbox: "Spam" }} />
      </MantineProvider>,
    );
    expect(screen.getByRole("button", { name: "Not spam" })).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Mark as spam" }),
    ).not.toBeInTheDocument();
  });

  it("toggles the star action from the reader toolbar", () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Star conversation" }));
    expect(props.onToggleStar).toHaveBeenCalledWith(
      expect.objectContaining({ id: "message-1" }),
      true,
    );
  });

  it("toggles the reader read action for unread and read conversations", async () => {
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Mark as read" }));
    expect(props.onToggleRead).toHaveBeenCalledWith(true);

    rerender(
      <MantineProvider>
        <Reader
          {...props}
          message={{ ...message, id: "message-2", is_read: true }}
        />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Mark as unread" }));
    expect(props.onToggleRead).toHaveBeenCalledWith(false);
  });

  it("offers offline translation even when AI is disconnected", () => {
    render(
      <MantineProvider>
        <Reader {...props} aiConnected={false} message={message} />
      </MantineProvider>,
    );

    expect(
      screen.getByRole("button", { name: "Translate to English" }),
    ).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Summarize" }),
    ).not.toBeInTheDocument();
  });

  it("translates the subject and plain-text body, then restores the original", async () => {
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    expect(
      await screen.findByText("Translated from Estonian on this device"),
    ).toBeVisible();
    expect(
      screen.getByRole("heading", { name: "EN: Weekly notes" }),
    ).toBeVisible();
    expect(screen.getByText("EN: Tere maailm")).toBeVisible();
    expect(translationMocks.translate).toHaveBeenNthCalledWith(
      1,
      "et",
      "Weekly notes",
      false,
    );
    expect(translationMocks.translate).toHaveBeenNthCalledWith(
      2,
      "et",
      "Tere maailm",
      false,
    );

    fireEvent.click(screen.getByRole("button", { name: "Show original" }));
    expect(screen.getByRole("heading", { name: "Weekly notes" })).toBeVisible();
    expect(screen.getByText("Tere maailm")).toBeVisible();
  });

  it("ignores stale translation results after switching conversations", async () => {
    let finishSubject: ((value: string) => void) | undefined;
    translationMocks.translate.mockImplementationOnce(
      () =>
        new Promise<string>((resolve) => {
          finishSubject = resolve;
        }),
    );
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    await waitFor(() => expect(translationMocks.translate).toHaveBeenCalled());

    const nextMessage = {
      ...message,
      id: "message-next",
      thread_id: "thread-next",
      subject: "Different conversation",
    };
    rerender(
      <MantineProvider>
        <Reader {...props} message={nextMessage} />
      </MantineProvider>,
    );
    finishSubject?.("Old translated subject");

    await waitFor(() =>
      expect(translationMocks.translate).toHaveBeenCalledTimes(2),
    );
    expect(
      screen.getByRole("heading", { name: "Different conversation" }),
    ).toBeVisible();
    expect(
      screen.queryByText("Translated from Estonian on this device"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText("Old translated subject"),
    ).not.toBeInTheDocument();
  });

  it("translates every message in the active conversation", async () => {
    const earlier = {
      ...message,
      id: "message-0",
      uid: 0,
      from_name: "Earlier Sender",
      body_text: "Varasem kiri",
    };
    vi.mocked(api.content).mockImplementation(async (id) => ({
      body_text: id === earlier.id ? "Esimene kiri" : "Teine kiri",
      attachments: [],
    }));
    render(
      <MantineProvider>
        <Reader {...props} message={message} messages={[earlier, message]} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    expect(await screen.findByText("EN: Teine kiri")).toBeVisible();
    expect(api.content).toHaveBeenCalledTimes(2);
    expect(api.content).toHaveBeenCalledWith("message-0", null);
    expect(api.content).toHaveBeenCalledWith("message-1", null);
    expect(translationMocks.translate).toHaveBeenCalledWith(
      "et",
      "Esimene kiri",
      false,
    );
    expect(translationMocks.translate).toHaveBeenCalledWith(
      "et",
      "Teine kiri",
      false,
    );
    fireEvent.click(
      screen.getByRole("button", {
        name: "Expand message from Earlier Sender",
      }),
    );
    expect(await screen.findByText("EN: Esimene kiri")).toBeVisible();
  });

  it("translates the fictional native-arrival HTML through the sandboxed document", async () => {
    const nativeArrivalHtml =
      '<p>Tere, see on turvaline fiktiivne kiri. Palun kinnita, et Dakia tõlgib selle sõnumi kohapeal ilma sisu võrku saatmata.</p><img src="http://127.0.0.1:40123/remote-image.svg?via=translation-arrival" alt="remote content remains blocked">';
    const translatedArrivalHtml =
      '<p>Hello, this is a safe fictional message. Please confirm that Dakia translates this message locally without sending content over the network.</p><img src="http://127.0.0.1:40123/remote-image.svg?via=translation-arrival" alt="remote content remains blocked">';
    vi.mocked(api.content).mockResolvedValue({
      body_text:
        "Tere, see on turvaline fiktiivne kiri. Palun kinnita, et Dakia tõlgib selle sõnumi kohapeal ilma sisu võrku saatmata.",
      body_html: nativeArrivalHtml,
      attachments: [],
    });
    translationMocks.translate
      .mockResolvedValueOnce("English subject")
      .mockResolvedValueOnce(translatedArrivalHtml);
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    const renderedMessage = await screen.findByRole("document", {
      name: "Weekly notes",
    });
    const currentEmailRoot = () => emailRendererDocument(renderedMessage);
    expect(translationMocks.translate).toHaveBeenLastCalledWith(
      "et",
      nativeArrivalHtml,
      true,
    );
    await waitFor(() =>
      expect(currentEmailRoot()?.textContent).toContain(
        "Hello, this is a safe fictional message.",
      ),
    );
    expect(currentEmailRoot()?.querySelector("img")?.hasAttribute("src")).toBe(
      false,
    );
    const scripts = currentEmailRoot()?.querySelectorAll("script");
    expect(scripts).toHaveLength(1);
    expect(scripts?.[0]?.getAttribute("data-dakia-renderer-token")).toMatch(
      /^[a-f0-9]{48}$/,
    );
    expect(scripts?.[0]?.textContent).toContain(
      "document.currentScript?.getAttribute",
    );
    expect(
      currentEmailRoot()?.querySelector(
        "script:not([data-dakia-renderer-token]), iframe, form",
      ),
    ).toBeNull();
  });

  it("downloads a missing pack with progress before translating", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    vi.mocked(api.installTranslationModel).mockImplementation(
      async (_source, onProgress) => {
        onProgress?.({
          source: "et",
          downloadedBytes: 10_500_000,
          totalBytes: 21_000_000,
          fileIndex: 1,
          fileCount: 3,
        });
        await Promise.resolve();
        return {
          source: "et",
          target: "en",
          modelPath: "/model",
          shortlistPath: "/shortlist",
          vocabPaths: ["/vocab"],
          config: {},
        };
      },
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(translationMocks.confirm.mock.calls[0][0]).toBe(
      "Download Estonian translation?",
    );
    expect(translationMocks.confirm.mock.calls[0][1]).toContain(
      "without an internet connection",
    );
    await waitFor(() =>
      expect(api.installTranslationModel).toHaveBeenCalledWith(
        "et",
        expect.any(Function),
      ),
    );
    expect(
      await screen.findByText("Translated from Estonian on this device"),
    ).toBeVisible();
  });

  it("shows the model prompt without waiting for remote message content", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    vi.mocked(api.content).mockReturnValue(new Promise(() => undefined));
    translationMocks.confirm.mockResolvedValue(false);
    const longThread = Array.from({ length: 50 }, (_, index) => ({
      ...message,
      id: `message-${index}`,
      uid: index,
      snippet: "Tere, palun vaadake kahjuteate üksikasju.",
    }));

    render(
      <MantineProvider>
        <Reader {...props} message={longThread.at(-1)} messages={longThread} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(api.installTranslationModel).not.toHaveBeenCalled();
  });

  it("does not download when the user declines the language pack", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    translationMocks.confirm.mockResolvedValue(false);
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );

    await waitFor(() =>
      expect(translationMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(api.installTranslationModel).not.toHaveBeenCalled();
    expect(translationMocks.translate).not.toHaveBeenCalled();
  });

  it("cancels an in-progress pack download through the native backend", async () => {
    vi.mocked(api.translationModels).mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_000_000,
        installed: false,
      },
    ]);
    let finishDownload:
      | ((
          value: Awaited<ReturnType<typeof api.installTranslationModel>>,
        ) => void)
      | undefined;
    vi.mocked(api.installTranslationModel).mockImplementation(
      (_source, onProgress) =>
        new Promise((resolve) => {
          finishDownload = resolve;
          onProgress?.({
            source: "et",
            downloadedBytes: 1_000,
            totalBytes: 21_000_000,
            fileIndex: 1,
            fileCount: 3,
          });
        }),
    );
    render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));

    expect(api.cancelTranslationModelInstall).toHaveBeenCalledWith("et");
    finishDownload?.({
      source: "et",
      target: "en",
      modelPath: "/model",
      shortlistPath: "/shortlist",
      vocabPaths: ["/vocab"],
      config: {},
    });
  });

  it("handles English, unsupported, and backend errors without exposing raw failures", async () => {
    translationMocks.detect.mockResolvedValueOnce({
      language: "en",
      languageName: "English",
      reliable: true,
    });
    const { rerender } = render(
      <MantineProvider>
        <Reader {...props} message={message} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText("This conversation is already in English."),
    ).toBeVisible();
    expect(api.translationModels).not.toHaveBeenCalled();

    translationMocks.detect.mockResolvedValueOnce({
      language: "ja",
      languageName: "Japanese",
      reliable: true,
    });
    rerender(
      <MantineProvider>
        <Reader {...props} message={{ ...message, id: "message-2" }} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText(
        /Offline translation from Japanese is not available/,
      ),
    ).toBeVisible();

    const consoleError = vi
      .spyOn(console, "error")
      .mockImplementation(() => undefined);
    translationMocks.translate.mockRejectedValueOnce(
      new Error("Aborted(). Build with -s ASSERTIONS=1 for more info."),
    );
    rerender(
      <MantineProvider>
        <Reader {...props} message={{ ...message, id: "message-3" }} />
      </MantineProvider>,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Translate to English" }),
    );
    expect(
      await screen.findByText("Offline translation failed."),
    ).toBeVisible();
    expect(screen.queryByText(/Aborted|ASSERTIONS=1/)).not.toBeInTheDocument();
    expect(consoleError).toHaveBeenCalled();
    consoleError.mockRestore();
  });
});
