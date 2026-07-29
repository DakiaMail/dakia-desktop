import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  type Mock,
  vi,
} from "vitest";
import "../i18n";
import type { Account } from "../types";
import { Composer } from "./Composer";

const nativeDropMocks = vi.hoisted(() => ({
  readDroppedFiles: vi.fn(),
  onDragDropEvent: vi.fn(),
  listen: vi.fn(),
  listeners: new Map<string, (event: { payload: string }) => void>(),
  disposers: [] as ReturnType<typeof vi.fn>[],
}));

vi.mock("../api", () => ({
  api: {
    readDroppedFiles: nativeDropMocks.readDroppedFiles,
  },
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: nativeDropMocks.onDragDropEvent,
    listen: nativeDropMocks.listen,
  }),
}));

const account: Account = {
  id: "account",
  email: "me@example.com",
  account_name: "me@example.com",
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

const props = {
  accounts: [account],
  seed: { to: "you@example.com" },
  aiConnected: false,
  onSend: vi.fn(),
  onAiDraft: vi.fn(async () => "Draft"),
};

const nativeAttachment = {
  filename: "native.pdf",
  mime_type: "application/pdf",
  content_base64: "bmF0aXZl",
  size_bytes: 6,
};

async function waitForNativeListeners() {
  await waitFor(() => {
    expect(nativeDropMocks.listeners.has("dakia://dropped-file-receipt")).toBe(
      true,
    );
    expect(nativeDropMocks.listeners.has("dakia://dropped-file-error")).toBe(
      true,
    );
  });
}

function emitNative(event: string, payload = "") {
  const listener = nativeDropMocks.listeners.get(event);
  if (!listener) throw new Error(`Missing native listener for ${event}`);
  listener({ payload });
}

beforeEach(() => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    value: {},
  });
  nativeDropMocks.listeners.clear();
  nativeDropMocks.disposers.length = 0;
  nativeDropMocks.readDroppedFiles.mockReset();
  nativeDropMocks.onDragDropEvent.mockReset();
  nativeDropMocks.listen.mockReset();
  nativeDropMocks.onDragDropEvent.mockImplementation(async () => {
    const dispose = vi.fn();
    nativeDropMocks.disposers.push(dispose);
    return dispose;
  });
  nativeDropMocks.listen.mockImplementation(
    async (event: string, listener: (event: { payload: string }) => void) => {
      nativeDropMocks.listeners.set(event, listener);
      const dispose = vi.fn();
      nativeDropMocks.disposers.push(dispose);
      return dispose;
    },
  );
});

afterEach(() => {
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("Composer send feedback", () => {
  it("keeps AI drafting hidden even when a provider is connected", () => {
    render(<Composer {...props} aiConnected sendState="idle" />);

    expect(screen.queryByRole("button", { name: "Draft with AI" })).toBeNull();
  });

  it("tabs from the subject directly into the message body", () => {
    render(<Composer {...props} sendState="idle" />);

    const subject = screen.getByLabelText("Subject");
    const body = screen.getByLabelText("Write your message…");
    const tabStops = Array.from(
      document.querySelectorAll<HTMLElement>(
        'input:not([disabled]), select:not([disabled]), button:not([disabled]), [contenteditable="true"]',
      ),
    );

    expect(tabStops[tabStops.indexOf(subject) + 1]).toBe(body);
  });

  it("disables the draft and animatable send control while sending", () => {
    render(<Composer {...props} sendState="sending" />);
    expect(screen.getByRole("button", { name: /Sending/ })).toBeDisabled();
    expect(screen.getByLabelText("Write your message…")).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(screen.getByLabelText("To")).toBeDisabled();
  });

  it("shows the sent completion state", () => {
    render(<Composer {...props} sendState="sent" />);
    expect(
      screen.getByRole("button", { name: /Message sent/ }),
    ).toHaveAttribute("data-send-state", "sent");
  });

  it("preserves the draft when sending returns to idle after a failure", () => {
    const { rerender } = render(<Composer {...props} sendState="idle" />);
    const editor = screen.getByLabelText("Write your message…");
    editor.innerHTML = "<p><strong>Keep</strong> this draft</p>";
    fireEvent.input(editor);
    rerender(<Composer {...props} sendState="sending" />);
    rerender(<Composer {...props} sendState="idle" />);
    expect(screen.getByLabelText("Write your message…")).toHaveTextContent(
      "Keep this draft",
    );
  });

  it("sends semantic HTML with a readable plain-text alternative", () => {
    const onSend = vi.fn();
    render(<Composer {...props} onSend={onSend} sendState="idle" />);
    const editor = screen.getByLabelText("Write your message…");
    editor.innerHTML =
      "<p>Hello <strong>there</strong></p><ul><li>One</li></ul>";
    fireEvent.input(editor);
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_html: "<p>Hello <strong>there</strong></p><ul><li>One</li></ul>",
        body_text: "Hello there\n• One",
      }),
    );
  });

  it("preserves edited Thunderbird quote markers and their plain-text alternative", () => {
    const onSend = vi.fn();
    const bodyHtml = [
      "<p><br></p>",
      '<div class="moz-cite-prefix">On July 19, Mara wrote:</div>',
      '<blockquote type="cite">Original body</blockquote>',
    ].join("");
    render(
      <Composer
        {...props}
        seed={{
          to: "sender@example.com",
          body: "Plain seed fallback",
          bodyHtml,
        }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    const editor = screen.getByLabelText("Write your message…");
    expect(editor.innerHTML).toBe(bodyHtml);
    editor.innerHTML = `<p>Authored text</p>${bodyHtml}`;
    fireEvent.input(editor);
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_html: expect.stringContaining('class="moz-cite-prefix"'),
      }),
    );
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_html: expect.stringContaining(
          '<blockquote type="cite">Original body</blockquote>',
        ),
        body_text: expect.stringContaining("Authored text"),
      }),
    );
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_text: expect.stringContaining("On July 19, Mara wrote:"),
      }),
    );
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_text: expect.stringContaining("> Original body"),
      }),
    );
  });

  it("sanitizes a hostile seeded reply before rendering and sends that unchanged sanitized HTML", () => {
    const onSend = vi.fn();
    const bodyHtml = [
      "<p>Valid <strong>text</strong></p>",
      '<div class="moz-cite-prefix" onclick="alert(\'xss\')">On July 19, Mara wrote:</div>',
      '<blockquote type="cite" onmouseover="alert(\'xss\')">Quoted plain text</blockquote>',
      "<script>alert('xss')</script>",
      '<img src="https://example.com/tracker.png" onerror="alert(\'xss\')">',
      "<p onfocus=\"alert('xss')\">Safe body text</p>",
      "<a href=\"javascript:alert('xss')\">Dangerous link</a>",
    ].join("");
    const sanitizedBodyHtml = [
      "<p>Valid <strong>text</strong></p>",
      '<div class="moz-cite-prefix">On July 19, Mara wrote:</div>',
      '<blockquote type="cite">Quoted plain text</blockquote>',
      "<p>Safe body text</p>",
      "Dangerous link",
    ].join("");

    render(
      <Composer
        {...props}
        seed={{ to: "sender@example.com", bodyHtml }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    const editor = screen.getByLabelText("Write your message…");
    expect(editor.innerHTML).toBe(sanitizedBodyHtml);

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_html: sanitizedBodyHtml,
        body_text:
          "Valid text\nOn July 19, Mara wrote:\n> Quoted plain text\nSafe body text\nDangerous link",
      }),
    );
  });

  it("keeps an empty bodyHtml seed instead of falling back to its plain-text body", () => {
    render(
      <Composer
        {...props}
        seed={{
          to: "sender@example.com",
          body: "Plain seed fallback",
          bodyHtml: "",
        }}
        sendState="idle"
      />,
    );

    expect(screen.getByLabelText("Write your message…").innerHTML).toBe("");
  });

  it("initializes and sends Reply All Cc recipients from the compose seed", () => {
    const onSend = vi.fn();
    render(
      <Composer
        {...props}
        seed={{ to: "sender@example.com", cc: "peer@example.com" }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    expect(screen.getByLabelText("Cc")).toHaveValue("peer@example.com");
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        to: ["sender@example.com"],
        cc: ["peer@example.com"],
      }),
    );
  });

  it("keeps quoted display-name commas intact when sending", () => {
    const onSend = vi.fn();
    render(
      <Composer
        {...props}
        seed={{ to: '"Doe, Jane" <jane@example.com>' }}
        onSend={onSend}
        sendState="idle"
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        to: ['"Doe, Jane" <jane@example.com>'],
      }),
    );
  });

  it("adds a selected file once and silently ignores a duplicate", async () => {
    const onSend = vi.fn();
    const { container } = render(
      <Composer {...props} onSend={onSend} sendState="idle" />,
    );
    const input = container.querySelector('input[type="file"]');
    expect(input).not.toBeNull();
    const file = new File(["same attachment"], "example.png", {
      type: "image/png",
    });

    fireEvent.change(input!, { target: { files: [file] } });
    await screen.findByText("example.png");
    fireEvent.change(input!, { target: { files: [file] } });

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() =>
      expect(onSend).toHaveBeenCalledWith(
        expect.objectContaining({
          attachments: [expect.objectContaining({ filename: "example.png" })],
        }),
      ),
    );
  });
});

describe("Composer native dropped-file receipts", () => {
  it("resolves a native receipt and includes its attachment in the draft", async () => {
    const onSend = vi.fn();
    nativeDropMocks.readDroppedFiles.mockResolvedValue([nativeAttachment]);
    render(<Composer {...props} onSend={onSend} sendState="idle" />);
    await waitForNativeListeners();

    act(() => emitNative("dakia://dropped-file-receipt", "receipt-123"));

    expect(await screen.findByText("native.pdf")).toBeInTheDocument();
    expect(nativeDropMocks.readDroppedFiles).toHaveBeenCalledOnce();
    expect(nativeDropMocks.readDroppedFiles).toHaveBeenCalledWith(
      "receipt-123",
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        attachments: [
          expect.objectContaining({
            filename: "native.pdf",
            content_base64: "bmF0aXZl",
          }),
        ],
      }),
    );
  });

  it("shows a read error for rejected receipts and recovers on a later receipt", async () => {
    nativeDropMocks.readDroppedFiles
      .mockRejectedValueOnce(new Error("receipt expired"))
      .mockResolvedValueOnce([nativeAttachment]);
    render(<Composer {...props} sendState="idle" />);
    await waitForNativeListeners();

    act(() => emitNative("dakia://dropped-file-receipt", "expired"));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Could not read one of the selected files.",
    );

    act(() => emitNative("dakia://dropped-file-receipt", "fresh"));
    expect(await screen.findByText("native.pdf")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(nativeDropMocks.readDroppedFiles.mock.calls).toEqual([
      ["expired"],
      ["fresh"],
    ]);
  });

  it("surfaces a native rejection event without attempting to redeem a receipt", async () => {
    render(<Composer {...props} sendState="idle" />);
    await waitForNativeListeners();

    act(() => emitNative("dakia://dropped-file-error"));

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Could not read one of the selected files.",
    );
    expect(nativeDropMocks.readDroppedFiles).not.toHaveBeenCalled();
  });

  it("does not redeem native receipts while sending", async () => {
    nativeDropMocks.readDroppedFiles.mockResolvedValue([nativeAttachment]);
    render(<Composer {...props} sendState="sending" />);
    await waitForNativeListeners();

    act(() => emitNative("dakia://dropped-file-receipt", "while-sending"));
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 10));
    });

    expect(nativeDropMocks.readDroppedFiles).not.toHaveBeenCalled();
    expect(screen.queryByText("native.pdf")).not.toBeInTheDocument();
  });

  it("suppresses the native receipt paired with an already handled browser drop", async () => {
    nativeDropMocks.readDroppedFiles.mockResolvedValue([nativeAttachment]);
    render(<Composer {...props} sendState="idle" />);
    await waitForNativeListeners();
    const browserFile = new File(["browser"], "browser.txt", {
      type: "text/plain",
    });

    fireEvent.drop(screen.getByRole("main"), {
      dataTransfer: {
        files: [browserFile],
        types: ["Files"],
      },
    });
    act(() => emitNative("dakia://dropped-file-receipt", "paired-receipt"));

    expect(await screen.findByText("browser.txt")).toBeInTheDocument();
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 10));
    });
    expect(nativeDropMocks.readDroppedFiles).not.toHaveBeenCalled();
    expect(screen.queryByText("native.pdf")).not.toBeInTheDocument();
  });

  it("disposes drag, receipt, and error listeners on unmount", async () => {
    const { unmount } = render(<Composer {...props} sendState="idle" />);
    await waitForNativeListeners();
    await act(async () => {
      await Promise.resolve();
    });
    expect(nativeDropMocks.disposers).toHaveLength(3);

    unmount();

    for (const dispose of nativeDropMocks.disposers) {
      expect(dispose).toHaveBeenCalledOnce();
    }
  });

  it("disposes listeners whose async registration finishes after unmount", async () => {
    const registrations: Array<{
      dispose: Mock<() => void>;
      resolve: (dispose: () => void) => void;
    }> = [];
    const deferredRegistration = () =>
      new Promise<() => void>((resolve) => {
        registrations.push({ dispose: vi.fn(), resolve });
      });
    nativeDropMocks.onDragDropEvent.mockImplementation(deferredRegistration);
    nativeDropMocks.listen.mockImplementation(deferredRegistration);

    const { unmount } = render(<Composer {...props} sendState="idle" />);
    expect(registrations).toHaveLength(3);
    unmount();

    await act(async () => {
      for (const registration of registrations) {
        registration.resolve(registration.dispose);
      }
      await Promise.resolve();
    });

    for (const registration of registrations) {
      expect(registration.dispose).toHaveBeenCalledOnce();
    }
  });
});
