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
import { StrictMode } from "react";
import "../i18n";
import type { Account } from "../types";
import { Composer } from "./Composer";

const nativeDropMocks = vi.hoisted(() => ({
  readDroppedFiles: vi.fn(),
  reduceComposeImage: vi.fn(),
  inspectComposeImage: vi.fn(),
  onDragDropEvent: vi.fn(),
  listen: vi.fn(),
  listeners: new Map<string, (event: { payload: string }) => void>(),
  disposers: [] as ReturnType<typeof vi.fn>[],
}));

const nativeFeedbackMocks = vi.hoisted(() => ({
  confirmNativeAction: vi.fn(),
}));

vi.mock("../api", () => ({
  api: {
    readDroppedFiles: nativeDropMocks.readDroppedFiles,
    reduceComposeImage: nativeDropMocks.reduceComposeImage,
    inspectComposeImage: nativeDropMocks.inspectComposeImage,
  },
}));

vi.mock("../nativeFeedback", () => ({
  confirmNativeAction: nativeFeedbackMocks.confirmNativeAction,
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

const TWO_MIB = 2 * 1024 * 1024;

const largeImage = (
  filename: string,
  size_bytes = TWO_MIB + 1,
  content_base64 = `base64-${filename}`,
) => ({
  filename,
  mime_type: "image/jpeg",
  content_base64,
  size_bytes,
});

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

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
  nativeDropMocks.reduceComposeImage.mockReset();
  nativeDropMocks.inspectComposeImage.mockReset();
  nativeDropMocks.inspectComposeImage.mockResolvedValue({
    eligible: true,
    reason: null,
  });
  nativeFeedbackMocks.confirmNativeAction.mockReset();
  nativeFeedbackMocks.confirmNativeAction.mockResolvedValue(false);
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

  it("renders and sends safe table and image layout in quoted rich email history", () => {
    const onSend = vi.fn();
    const bodyHtml = [
      "<p><br></p>",
      '<div class="moz-cite-prefix">GitHub wrote:</div>',
      '<blockquote type="cite"><div data-dakia-quoted-email="true">',
      '<table width="600" style="width: 600px; background-color: rgb(255, 255, 255)"><tbody><tr>',
      '<td align="center" style="padding: 24px"><img alt="GitHub" width="32" src="https://example.com/github.png">',
      '<a href="https://example.com/settings" style="display: inline-block; background-color: rgb(22, 136, 63); padding: 12px; color: white">Manage budgets</a></td>',
      "</tr></tbody></table></div></blockquote>",
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
    expect(editor.innerHTML).toContain('<table width="600"');
    expect(editor.innerHTML).toContain('<img alt="GitHub" width="32"');
    expect(editor.innerHTML).toContain("background-color: rgb(255, 255, 255)");
    expect(editor.innerHTML).toContain("background-color: rgb(22, 136, 63)");

    editor.innerHTML = `<p>Authored text</p>${editor.innerHTML}`;
    fireEvent.input(editor);
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_html: expect.stringContaining('<table width="600"'),
        body_text: expect.stringContaining("Authored text"),
      }),
    );
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        body_text: expect.stringContaining("Manage budgets"),
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

  it("falls back to the plain body when sanitizing seeded HTML removes everything", () => {
    render(
      <Composer
        {...props}
        seed={{
          to: "recipient@example.com",
          body: "Accessible image description",
          bodyHtml: '<img src="cid:chart" alt="Accessible image description">',
        }}
        sendState="idle"
      />,
    );

    expect(screen.getByLabelText("Write your message…")).toHaveTextContent(
      "Accessible image description",
    );
  });

  it("initializes and sends Cc and Bcc recipients from the compose seed", () => {
    const onSend = vi.fn();
    render(
      <Composer
        {...props}
        seed={{
          to: "sender@example.com",
          cc: "peer@example.com",
          bcc: "hidden@example.com",
        }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    expect(screen.getByLabelText("Cc")).toHaveValue("peer@example.com");
    expect(screen.getByLabelText("Bcc")).toHaveValue("hidden@example.com");
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        to: ["sender@example.com"],
        cc: ["peer@example.com"],
        bcc: ["hidden@example.com"],
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

describe("Composer large image attachment review", () => {
  it("finishes image review under the production StrictMode lifecycle", async () => {
    render(
      <StrictMode>
        <Composer
          {...props}
          seed={{
            to: "you@example.com",
            attachments: [largeImage("strict.jpg")],
          }}
          sendState="idle"
        />
      </StrictMode>,
    );

    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled(),
    );
    expect(
      screen.getByRole("button", { name: "Remove strict.jpg" }),
    ).toBeEnabled();
  });

  it("does not offer reduction at the exact 2 MiB boundary", async () => {
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [largeImage("boundary.jpg", TWO_MIB)],
        }}
        sendState="idle"
      />,
    );

    expect(await screen.findByText("boundary.jpg")).toBeInTheDocument();
    await act(async () => {
      await Promise.resolve();
    });
    expect(nativeFeedbackMocks.confirmNativeAction).not.toHaveBeenCalled();
    expect(nativeDropMocks.inspectComposeImage).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled();
  });

  it("uses native byte inspection for a forwarded image with generic MIME metadata", async () => {
    const forwarded = {
      ...largeImage("forwarded.jpg"),
      mime_type: "application/octet-stream",
    };
    render(
      <Composer
        {...props}
        seed={{ to: "you@example.com", attachments: [forwarded] }}
        sendState="idle"
      />,
    );

    await waitFor(() =>
      expect(nativeDropMocks.inspectComposeImage).toHaveBeenCalledWith(
        forwarded,
      ),
    );
    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
    );
  });

  it("does not offer reduction after native inspection rejects a large non-image", async () => {
    nativeDropMocks.inspectComposeImage.mockResolvedValue({
      eligible: false,
      reason: "unsupported_format",
    });
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [
            {
              ...largeImage("document.pdf"),
              mime_type: "application/pdf",
            },
          ],
        }}
        sendState="idle"
      />,
    );

    await waitFor(() =>
      expect(nativeDropMocks.inspectComposeImage).toHaveBeenCalledOnce(),
    );
    expect(nativeFeedbackMocks.confirmNativeAction).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled(),
    );
  });

  it.each(["picker", "browser drop"])(
    "offers reduction for a large image added through the %s path",
    async (source) => {
      const file = new File(["image"], `${source}.jpg`, {
        type: "image/jpeg",
      });
      Object.defineProperty(file, "size", { value: TWO_MIB + 1 });
      const { container } = render(<Composer {...props} sendState="idle" />);

      if (source === "picker") {
        fireEvent.change(container.querySelector('input[type="file"]')!, {
          target: { files: [file] },
        });
      } else {
        fireEvent.drop(screen.getByRole("main"), {
          dataTransfer: { files: [file], types: ["Files"] },
        });
      }

      await waitFor(() =>
        expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
      );
    },
  );

  it("groups large images added together into one native offer", async () => {
    const choice = deferred<boolean>();
    nativeFeedbackMocks.confirmNativeAction.mockReturnValue(choice.promise);
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [largeImage("first.jpg"), largeImage("second.jpg")],
        }}
        sendState="idle"
      />,
    );

    expect(await screen.findByText("first.jpg")).toBeInTheDocument();
    expect(screen.getByText("second.jpg")).toBeInTheDocument();
    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledWith(
        "Reduce image size?",
        "2 attached images are larger than 2 MB. Reduce them to make this message smaller? They will be limited to 1600 px and may lose some detail.",
        "Reduce size",
        "Keep originals",
      ),
    );
    expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce();

    await act(async () => choice.resolve(false));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled(),
    );
  });

  it("keeps originals and does not offer them again", async () => {
    const onSend = vi.fn();
    const original = largeImage("kept.jpg", 3 * 1024 * 1024, "original-data");
    const { rerender } = render(
      <Composer
        {...props}
        seed={{ to: "you@example.com", attachments: [original] }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled(),
    );
    rerender(
      <Composer
        {...props}
        seed={{ to: "you@example.com", attachments: [original] }}
        onSend={onSend}
        sendState="idle"
      />,
    );
    await act(async () => {
      await Promise.resolve();
    });
    expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce();
    expect(nativeDropMocks.reduceComposeImage).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        attachments: [
          {
            filename: "kept.jpg",
            mime_type: "image/jpeg",
            content_base64: "original-data",
          },
        ],
      }),
    );
  });

  it("replaces a reduced image, shows the size saving, and sends the new bytes", async () => {
    const onSend = vi.fn();
    nativeFeedbackMocks.confirmNativeAction.mockResolvedValue(true);
    nativeDropMocks.reduceComposeImage.mockResolvedValue({
      status: "reduced",
      attachment: largeImage("photo-reduced.jpg", 1024 * 1024, "reduced-data"),
      original_size_bytes: 3 * 1024 * 1024,
      reduced_size_bytes: 1024 * 1024,
    });
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [largeImage("photo.jpg", 3 * 1024 * 1024, "old-data")],
        }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    expect(await screen.findByText("photo-reduced.jpg")).toBeInTheDocument();
    expect(screen.getByText("3.0 MB → 1.0 MB")).toBeInTheDocument();
    expect(nativeDropMocks.reduceComposeImage).toHaveBeenCalledWith(
      largeImage("photo.jpg", 3 * 1024 * 1024, "old-data"),
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    expect(onSend).toHaveBeenCalledWith(
      expect.objectContaining({
        attachments: [
          {
            filename: "photo-reduced.jpg",
            mime_type: "image/jpeg",
            content_base64: "reduced-data",
          },
        ],
      }),
    );
  });

  it.each([
    {
      label: "failure",
      arrange: () =>
        nativeDropMocks.reduceComposeImage.mockRejectedValue(
          new Error("decoder failed"),
        ),
      expected: "problem.jpg could not be made smaller. The original was kept.",
    },
    {
      label: "unchanged result",
      arrange: () =>
        nativeDropMocks.reduceComposeImage.mockResolvedValue({
          status: "unchanged",
          reason: "not_smaller",
          attachment: null,
          original_size_bytes: TWO_MIB + 1,
          reduced_size_bytes: null,
        }),
      expected: "problem.jpg could not be made smaller. The original was kept.",
    },
  ])(
    "keeps the original and explains a reduction $label",
    async ({ arrange, expected }) => {
      nativeFeedbackMocks.confirmNativeAction.mockResolvedValue(true);
      arrange();
      render(
        <Composer
          {...props}
          seed={{
            to: "you@example.com",
            attachments: [largeImage("problem.jpg")],
          }}
          sendState="idle"
        />,
      );

      expect(await screen.findByRole("status")).toHaveTextContent(expected);
      expect(screen.getByText("problem.jpg")).toBeInTheDocument();
      expect(screen.getByText("2.0 MB")).toBeInTheDocument();
    },
  );

  it("names every original kept when a mixed batch cannot be reduced", async () => {
    nativeFeedbackMocks.confirmNativeAction.mockResolvedValue(true);
    nativeDropMocks.reduceComposeImage
      .mockResolvedValueOnce({
        status: "unchanged",
        reason: "not_smaller",
        attachment: null,
        original_size_bytes: TWO_MIB + 1,
        reduced_size_bytes: null,
      })
      .mockRejectedValueOnce(new Error("decoder failed"));
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [largeImage("first.jpg"), largeImage("second.jpg")],
        }}
        sendState="idle"
      />,
    );

    expect(await screen.findByRole("status")).toHaveTextContent(
      "2 images could not be made smaller. Their originals were kept: first.jpg, second.jpg.",
    );
  });

  it("offers again after a kept image is removed and re-added", async () => {
    const image = largeImage("again.jpg");
    nativeDropMocks.readDroppedFiles.mockResolvedValue([image]);
    render(
      <Composer
        {...props}
        seed={{ to: "you@example.com", attachments: [image] }}
        sendState="idle"
      />,
    );

    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
    );
    const remove = await screen.findByRole("button", {
      name: "Remove again.jpg",
    });
    await waitFor(() => expect(remove).toBeEnabled());
    fireEvent.click(remove);
    expect(screen.queryByText("again.jpg")).not.toBeInTheDocument();

    await waitForNativeListeners();
    act(() => emitNative("dakia://dropped-file-receipt", "re-added"));
    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledTimes(2),
    );
  });

  it("offers once for each later attachment batch", async () => {
    nativeDropMocks.readDroppedFiles
      .mockResolvedValueOnce([largeImage("batch-one.jpg")])
      .mockResolvedValueOnce([largeImage("batch-two.jpg")]);
    render(<Composer {...props} sendState="idle" />);
    await waitForNativeListeners();

    act(() => emitNative("dakia://dropped-file-receipt", "batch-one"));
    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledOnce(),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Remove batch-one.jpg" }),
      ).toBeEnabled(),
    );

    act(() => emitNative("dakia://dropped-file-receipt", "batch-two"));
    await waitFor(() =>
      expect(nativeFeedbackMocks.confirmNativeAction).toHaveBeenCalledTimes(2),
    );
    expect(await screen.findByText("batch-two.jpg")).toBeInTheDocument();
  });

  it("blocks button and keyboard sending while the decision or reduction is pending", async () => {
    const onSend = vi.fn();
    const choice = deferred<boolean>();
    const reduction = deferred<{
      status: "unchanged";
      original_size_bytes: number;
    }>();
    nativeFeedbackMocks.confirmNativeAction.mockReturnValue(choice.promise);
    nativeDropMocks.reduceComposeImage.mockReturnValue(reduction.promise);
    render(
      <Composer
        {...props}
        seed={{
          to: "you@example.com",
          attachments: [largeImage("pending.jpg")],
        }}
        onSend={onSend}
        sendState="idle"
      />,
    );

    const send = screen.getByRole("button", { name: /^Send/ });
    await waitFor(() => expect(send).toBeDisabled());
    expect(screen.getByRole("button", { name: "Attach" })).toBeDisabled();
    fireEvent.click(send);
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(onSend).not.toHaveBeenCalled();

    await act(async () => choice.resolve(true));
    await waitFor(() =>
      expect(nativeDropMocks.reduceComposeImage).toHaveBeenCalledOnce(),
    );
    expect(screen.getByRole("status")).toHaveTextContent("Reducing images…");
    expect(send).toBeDisabled();
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(onSend).not.toHaveBeenCalled();

    await act(async () =>
      reduction.resolve({
        status: "unchanged",
        original_size_bytes: TWO_MIB + 1,
      }),
    );
    await waitFor(() => expect(send).toBeEnabled());
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
