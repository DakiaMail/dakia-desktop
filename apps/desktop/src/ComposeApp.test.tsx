import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { Account } from "./types";
import { ComposeApp } from "./ComposeApp";

const mocks = vi.hoisted(() => {
  class SendMessageError extends Error {
    constructor(
      readonly kind:
        "not_submitted" | "delivery_uncertain" | "sent_copy_failed",
    ) {
      super("safe send failure");
    }
  }
  return {
    SendMessageError,
    api: {
      accounts: vi.fn(),
      aiAvailable: vi.fn(),
      draft: vi.fn(),
      forwardAttachments: vi.fn(),
      readDroppedFiles: vi.fn(),
      readSendRecoverySeed: vi.fn(),
      acknowledgeSendRecoverySeed: vi.fn(),
      send: vi.fn(),
    },
    closeComposeWindow: vi.fn(),
    discardComposeSeed: vi.fn(),
    notifyOutbox: vi.fn(),
    persistComposeSeed: vi.fn(),
    readDatabaseComposeSeed: vi.fn(),
    readComposeSeed: vi.fn(),
    composeSeedFromSendRecovery: vi.fn(),
    hasNativeComposeRecovery: vi.fn(),
    hasNativeComposeSeedHandoff: vi.fn(),
    readNativeComposeRecoveryKey: vi.fn(),
    notifyComposeWindowReady: vi.fn(),
    onComposeTarget: vi.fn(),
    rememberComposeSeed: vi.fn(),
    showNativeMessage: vi.fn(),
  };
});

vi.mock("./api", () => ({
  api: mocks.api,
  SendMessageError: mocks.SendMessageError,
}));
vi.mock("./composeWindow", () => ({
  closeComposeWindow: mocks.closeComposeWindow,
  discardComposeSeed: mocks.discardComposeSeed,
  notifyOutbox: mocks.notifyOutbox,
  persistComposeSeed: mocks.persistComposeSeed,
  readDatabaseComposeSeed: mocks.readDatabaseComposeSeed,
  readComposeSeed: mocks.readComposeSeed,
  composeSeedFromSendRecovery: mocks.composeSeedFromSendRecovery,
  hasNativeComposeRecovery: mocks.hasNativeComposeRecovery,
  hasNativeComposeSeedHandoff: mocks.hasNativeComposeSeedHandoff,
  readNativeComposeRecoveryKey: mocks.readNativeComposeRecoveryKey,
  notifyComposeWindowReady: mocks.notifyComposeWindowReady,
  onComposeTarget: mocks.onComposeTarget,
  rememberComposeSeed: mocks.rememberComposeSeed,
}));
vi.mock("./nativeFeedback", () => ({
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: true }));

const account: Account = {
  id: "account-1",
  email: "me@example.com",
  account_name: "Personal",
  display_name: "Me",
  provider_id: "fastmail",
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function renderComposer() {
  render(<ComposeApp />);
  await screen.findByRole("button", { name: /^Send/ });
}

function fillDraft() {
  fireEvent.change(screen.getByLabelText("To"), {
    target: { value: "one@example.com, two@example.com" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Cc · Bcc" }));
  fireEvent.change(screen.getByLabelText("Cc"), {
    target: { value: "copy@example.com" },
  });
  fireEvent.change(screen.getByLabelText("Bcc"), {
    target: { value: "hidden@example.com" },
  });
  fireEvent.change(screen.getByLabelText("Subject"), {
    target: { value: "Exact draft" },
  });
  const body = screen.getByLabelText("Write your message…");
  body.innerHTML = "<p>Hello <strong>there</strong></p>";
  fireEvent.input(body);
}

describe("ComposeApp send lifecycle", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    mocks.api.accounts.mockResolvedValue([account]);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.draft.mockResolvedValue("");
    mocks.api.forwardAttachments.mockResolvedValue([]);
    mocks.api.readDroppedFiles.mockResolvedValue([]);
    mocks.readDatabaseComposeSeed.mockResolvedValue(undefined);
    mocks.hasNativeComposeRecovery.mockReturnValue(false);
    mocks.hasNativeComposeSeedHandoff.mockReturnValue(false);
    mocks.onComposeTarget.mockResolvedValue(() => undefined);
    mocks.notifyComposeWindowReady.mockResolvedValue(undefined);
    mocks.readComposeSeed.mockReturnValue({});
    mocks.rememberComposeSeed.mockReturnValue(true);
    mocks.notifyOutbox.mockResolvedValue(undefined);
    mocks.closeComposeWindow.mockResolvedValue(undefined);
    localStorage.clear();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("publishes the exact draft to Outbox immediately and suppresses duplicate pending sends", async () => {
    const submission = deferred<string>();
    mocks.api.send.mockReturnValue(submission.promise);
    await renderComposer();
    fillDraft();

    const send = screen.getByRole("button", { name: /^Send/ });
    act(() => {
      fireEvent.click(send);
      fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    });

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledTimes(1));
    expect(mocks.api.send).toHaveBeenCalledWith({
      account_id: "account-1",
      to: ["one@example.com", "two@example.com"],
      cc: ["copy@example.com"],
      bcc: ["hidden@example.com"],
      subject: "Exact draft",
      body_text: "Hello there",
      body_html: "<p>Hello <strong>there</strong></p>",
      in_reply_to: null,
      references: null,
      attachments: [],
      send_attempt_id: expect.any(String),
    });
    expect(mocks.notifyOutbox).toHaveBeenCalledWith(
      expect.objectContaining({
        phase: "sending",
        message: expect.objectContaining({
          account_id: "account-1",
          mailbox: "Outbox",
          subject: "Exact draft",
          to_addresses: "one@example.com, two@example.com",
          body_text: "Hello there",
        }),
      }),
    );
    const attemptId = mocks.api.send.mock.calls[0]?.[0]?.send_attempt_id;
    expect(mocks.notifyOutbox).toHaveBeenCalledWith(
      expect.objectContaining({
        phase: "sending",
        message: expect.objectContaining({
          id: `outbox-account-1-${attemptId}`,
        }),
      }),
    );
    expect(screen.getByRole("button", { name: /Sending/ })).toBeDisabled();

    submission.resolve("queued");
    await waitFor(() =>
      expect(mocks.notifyOutbox).toHaveBeenCalledWith(
        expect.objectContaining({ phase: "finished" }),
      ),
    );
    expect(mocks.discardComposeSeed).toHaveBeenCalledOnce();
  });

  it("keeps an explicit plain-text-only body null while saving a recovery draft", async () => {
    mocks.readComposeSeed.mockReturnValue({
      sendAttemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
      accountId: account.id,
      to: "recover@example.test",
      subject: "Plain recovery",
      body: "Only plain text",
      bodyHtml: null,
    });
    render(<ComposeApp />);

    await screen.findByRole("button", { name: /^Send/ });
    await waitFor(() =>
      expect(mocks.persistComposeSeed).toHaveBeenCalledWith(
        expect.objectContaining({
          sendAttemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
          body: "Only plain text",
          bodyHtml: null,
        }),
      ),
    );
  });

  it("does not send a recovered draft whose original account has been disabled", async () => {
    mocks.readComposeSeed.mockReturnValue({
      sendAttemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
      accountId: account.id,
      to: "recover@example.test",
      subject: "Disabled account recovery",
    });
    mocks.api.accounts.mockResolvedValue([{ ...account, enabled: false }]);
    render(<ComposeApp />);

    const send = await screen.findByRole("button", { name: /^Send/ });
    expect(send).toBeDisabled();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Connect an account before composing.",
    );
    expect(mocks.api.send).not.toHaveBeenCalled();
  });

  it("does not invoke SMTP until the sending draft is durably committed", async () => {
    const sendingCommit = deferred<void>();
    const submission = deferred<string>();
    mocks.api.send.mockReturnValue(submission.promise);
    await renderComposer();
    fillDraft();
    mocks.persistComposeSeed.mockClear();
    mocks.persistComposeSeed.mockReturnValue(sendingCommit.promise);

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    // Let an incorrectly unblocked send continuation run while the durable
    // commit is still pending. A synchronous assertion alone can miss it.
    await act(async () => {
      await Promise.resolve();
    });

    expect(mocks.notifyOutbox).toHaveBeenCalledWith(
      expect.objectContaining({ phase: "sending" }),
    );
    expect(mocks.api.send).not.toHaveBeenCalled();
    expect(mocks.persistComposeSeed).toHaveBeenCalledWith(
      expect.objectContaining({
        subject: "Exact draft",
        bodyHtml: "<p>Hello <strong>there</strong></p>",
        sendState: "sending",
      }),
    );

    sendingCommit.resolve();
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    submission.resolve("queued");
  });

  it("does not invoke SMTP when the sending draft cannot be committed", async () => {
    await renderComposer();
    fillDraft();
    mocks.persistComposeSeed.mockClear();
    mocks.persistComposeSeed.mockRejectedValue(new Error("quota exceeded"));

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Dakia could not save this draft. Keep this window open while you check storage.",
        "error",
      ),
    );
    expect(mocks.api.send).not.toHaveBeenCalled();
    expect(mocks.notifyOutbox).toHaveBeenCalledWith(
      expect.objectContaining({ phase: "finished" }),
    );
    expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled();
  });

  it("keeps a not-submitted draft editable and permits a deliberate retry", async () => {
    mocks.api.send.mockRejectedValueOnce(
      new mocks.SendMessageError("not_submitted"),
    );
    mocks.api.send.mockResolvedValueOnce("queued");
    await renderComposer();
    fillDraft();

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Could not send message",
        "safe send failure",
        "error",
      ),
    );
    expect(screen.getByLabelText("To")).toHaveValue(
      "one@example.com, two@example.com",
    );
    expect(screen.getByLabelText("Subject")).toHaveValue("Exact draft");
    expect(screen.getByLabelText("Write your message…")).toHaveTextContent(
      "Hello there",
    );
    expect(screen.getByRole("button", { name: /^Send/ })).toBeEnabled();

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledTimes(2));
    expect(mocks.api.send.mock.calls[1][0].send_attempt_id).toBe(
      mocks.api.send.mock.calls[0][0].send_attempt_id,
    );
  });

  it("uses the same seeded send attempt in two windows for one compose action", async () => {
    const sendAttemptId = "0f91b7ad-0d42-4afc-b7eb-4abec2a55b2c";
    const submission = deferred<string>();
    mocks.readComposeSeed.mockReturnValue({
      to: "you@example.com",
      sendAttemptId,
    });
    mocks.api.send.mockReturnValue(submission.promise);
    render(
      <>
        <ComposeApp />
        <ComposeApp />
      </>,
    );
    await waitFor(() =>
      expect(screen.getAllByRole("button", { name: /^Send/ })).toHaveLength(2),
    );

    for (const send of screen.getAllByRole("button", { name: /^Send/ })) {
      fireEvent.click(send);
    }

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledTimes(2));
    expect(mocks.api.send.mock.calls).toEqual([
      [expect.objectContaining({ send_attempt_id: sendAttemptId })],
      [expect.objectContaining({ send_attempt_id: sendAttemptId })],
    ]);
  });

  it("uses the captured mailbox epoch when preparing forward attachments", async () => {
    mocks.readComposeSeed.mockReturnValue({
      forwardMessageId: "message-1",
      forwardMessageUidValidity: 90210,
    });
    await renderComposer();

    expect(mocks.api.forwardAttachments).toHaveBeenCalledWith(
      "message-1",
      90210,
    );
  });

  it("sends the captured context receipts to AI drafting", async () => {
    mocks.readComposeSeed.mockReturnValue({
      contextMessageIds: ["message-77"],
      contextMessageTargets: [
        { messageId: "message-77", expectedUidValidity: 77 },
      ],
    });
    mocks.api.aiAvailable.mockResolvedValue(true);
    mocks.api.draft.mockResolvedValue("AI draft text");
    await renderComposer();

    fireEvent.click(
      await screen.findByRole("button", { name: "Draft with AI" }),
    );

    await waitFor(() =>
      expect(mocks.api.draft).toHaveBeenCalledWith(
        expect.objectContaining({ provider: "ollama" }),
        [{ messageId: "message-77", expectedUidValidity: 77 }],
        expect.any(String),
      ),
    );
  });

  it("does not release legacy ID-only context to AI drafting", async () => {
    mocks.readComposeSeed.mockReturnValue({
      contextMessageIds: ["legacy-message"],
      body: "Keep this draft",
    });
    mocks.api.aiAvailable.mockResolvedValue(true);
    await renderComposer();

    fireEvent.click(
      await screen.findByRole("button", { name: "Draft with AI" }),
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Could not prepare AI draft",
        "Dakia needs the current version of this email before it can share it with AI.",
        "error",
      ),
    );
    expect(mocks.api.draft).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Write your message…")).toHaveTextContent(
      "Keep this draft",
    );
  });

  it.each([
    [{ messageId: "message-77", expectedUidValidity: null }],
    [{ messageId: "different-message", expectedUidValidity: 77 }],
  ])("fails closed when a context receipt is invalid: %o", async (targets) => {
    mocks.readComposeSeed.mockReturnValue({
      contextMessageIds: ["message-77"],
      contextMessageTargets: targets,
    });
    mocks.api.aiAvailable.mockResolvedValue(true);
    await renderComposer();

    fireEvent.click(
      await screen.findByRole("button", { name: "Draft with AI" }),
    );

    await waitFor(() => expect(mocks.showNativeMessage).toHaveBeenCalled());
    expect(mocks.api.draft).not.toHaveBeenCalled();
  });

  it("restores an uncertain draft and attachment after reload without another submission", async () => {
    const seed = {
      sendAttemptId: "0910394c-4c39-4258-bcc5-dbe1c87473cf",
      accountId: "account-1",
      to: "you@example.com",
      subject: "Keep this draft",
      bodyHtml: "<p>Reload body</p>",
      attachments: [
        {
          filename: "report.pdf",
          mime_type: "application/pdf",
          content_base64: "cGRm",
          size_bytes: 3,
        },
      ],
    };
    mocks.readComposeSeed.mockReturnValue(seed);
    mocks.api.send.mockRejectedValue(
      new mocks.SendMessageError("delivery_uncertain"),
    );
    const first = render(<ComposeApp />);
    await screen.findByRole("button", { name: /^Send/ });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() =>
      expect(mocks.persistComposeSeed).toHaveBeenLastCalledWith(
        expect.objectContaining({
          sendAttemptId: seed.sendAttemptId,
          subject: "Keep this draft",
          bodyHtml: "<p>Reload body</p>",
          attachments: seed.attachments,
          sendState: "delivery_uncertain",
        }),
      ),
    );
    const persisted = mocks.persistComposeSeed.mock.calls.at(-1)?.[0];
    first.unmount();
    mocks.readComposeSeed.mockReturnValue(persisted);

    render(<ComposeApp />);
    expect(
      await screen.findByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    expect(screen.getByLabelText("Subject")).toHaveValue("Keep this draft");
    // The status button is mounted before the rich-text editor's passive
    // initialization effect. Wait for the restored content itself.
    await waitFor(() =>
      expect(screen.getByLabelText("Write your message…")).toHaveTextContent(
        "Reload body",
      ),
    );
    expect(screen.getByText("report.pdf")).toBeInTheDocument();
    expect(mocks.api.send).toHaveBeenCalledTimes(1);
  });

  it("commits an uncertain terminal state before it can be restored after reload", async () => {
    const seed = {
      sendAttemptId: "1f7eb0d5-8840-44f2-aa0e-0282b08dbe4f",
      accountId: "account-1",
      to: "you@example.com",
      subject: "Durable result",
      attachments: [
        {
          filename: "report.pdf",
          mime_type: "application/pdf",
          content_base64: "cGRm",
          size_bytes: 3,
        },
      ],
    };
    const terminalCommit = deferred<void>();
    mocks.readComposeSeed.mockReturnValue(seed);
    mocks.api.send.mockRejectedValue(
      new mocks.SendMessageError("delivery_uncertain"),
    );
    const first = render(<ComposeApp />);
    await screen.findByRole("button", { name: /^Send/ });
    mocks.persistComposeSeed.mockClear();
    mocks.persistComposeSeed.mockImplementation((nextSeed) =>
      nextSeed.sendState === "delivery_uncertain"
        ? terminalCommit.promise
        : Promise.resolve(),
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    await waitFor(() =>
      expect(mocks.persistComposeSeed).toHaveBeenCalledWith(
        expect.objectContaining({
          sendAttemptId: seed.sendAttemptId,
          subject: "Durable result",
          attachments: seed.attachments,
          sendState: "delivery_uncertain",
        }),
      ),
    );
    expect(screen.getByRole("button", { name: /Sending/ })).toBeDisabled();

    terminalCommit.resolve();
    await screen.findByRole("button", { name: /Delivery status unknown/ });
    const persisted = mocks.persistComposeSeed.mock.calls
      .map(([nextSeed]) => nextSeed)
      .find((nextSeed) => nextSeed.sendState === "delivery_uncertain");
    expect(persisted).toEqual(
      expect.objectContaining({
        sendAttemptId: seed.sendAttemptId,
        attachments: seed.attachments,
        sendState: "delivery_uncertain",
      }),
    );

    first.unmount();
    mocks.readComposeSeed.mockReturnValue(persisted);
    render(<ComposeApp />);
    expect(
      await screen.findByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    expect(mocks.api.send).toHaveBeenCalledOnce();
  });

  it("restores a sending draft as delivery-uncertain after reload before an acknowledgement", async () => {
    const seed = {
      sendAttemptId: "ab059ced-1711-430f-945f-5eff6473ecce",
      accountId: "account-1",
      to: "you@example.com",
      subject: "Awaiting server",
    };
    const submission = deferred<string>();
    mocks.readComposeSeed.mockReturnValue(seed);
    mocks.api.send.mockReturnValue(submission.promise);
    const first = render(<ComposeApp />);
    await screen.findByRole("button", { name: /^Send/ });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() =>
      expect(mocks.persistComposeSeed).toHaveBeenLastCalledWith(
        expect.objectContaining({ sendState: "sending" }),
      ),
    );
    const persisted = mocks.persistComposeSeed.mock.calls.at(-1)?.[0];
    first.unmount();
    mocks.readComposeSeed.mockReturnValue(persisted);

    render(<ComposeApp />);
    expect(
      await screen.findByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    expect(screen.getByLabelText("Subject")).toHaveValue("Awaiting server");
    expect(mocks.api.send).toHaveBeenCalledTimes(1);
  });

  it.each([
    [
      "delivery_uncertain",
      "Delivery status unknown",
      "Dakia did not receive confirmation from the mail server. Check Sent before sending again.",
    ],
    [
      "sent_copy_failed",
      "Message sent, but saving it to Sent failed",
      "The mail server accepted this message, but Dakia could not save a copy in Sent. Do not send it again.",
    ],
  ] as const)(
    "preserves the draft and blocks ordinary retry after %s",
    async (kind, title, detail) => {
      mocks.api.send.mockRejectedValue(new mocks.SendMessageError(kind));
      await renderComposer();
      fillDraft();

      fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

      await waitFor(() =>
        expect(mocks.showNativeMessage).toHaveBeenCalledWith(
          title,
          detail,
          "error",
        ),
      );
      expect(screen.getByLabelText("Subject")).toHaveValue("Exact draft");
      expect(
        screen.getByRole("button", { name: new RegExp(title) }),
      ).toBeDisabled();
      fireEvent.keyDown(window, { key: "Enter", metaKey: true });
      expect(mocks.api.send).toHaveBeenCalledTimes(1);
    },
  );

  it("does not enable retry when an untyped bridge failure loses the send outcome", async () => {
    mocks.api.send.mockRejectedValue(new Error("connection closed"));
    await renderComposer();
    fillDraft();

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Delivery status unknown",
        "Dakia did not receive confirmation from the mail server. Check Sent before sending again.",
        "error",
      ),
    );
    expect(
      screen.getByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(mocks.api.send).toHaveBeenCalledTimes(1);
  });

  it("keeps retry blocked when an uncertain terminal state cannot be committed", async () => {
    mocks.api.send.mockRejectedValue(
      new mocks.SendMessageError("delivery_uncertain"),
    );
    await renderComposer();
    fillDraft();
    mocks.persistComposeSeed.mockClear();
    mocks.persistComposeSeed.mockImplementation((seed) =>
      seed.sendState === "delivery_uncertain"
        ? Promise.reject(new Error("quota exceeded"))
        : Promise.resolve(),
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Dakia could not save this draft. Keep this window open while you check storage.",
        "error",
      ),
    );
    expect(
      screen.getByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    expect(mocks.api.send).toHaveBeenCalledOnce();
  });

  it("shows one safe error when the active draft cannot be persisted", async () => {
    mocks.persistComposeSeed.mockRejectedValue(
      new Error("storage unavailable"),
    );
    await renderComposer();

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Dakia could not save this draft. Keep this window open while you check storage.",
        "error",
      ),
    );
    expect(mocks.showNativeMessage).toHaveBeenCalledTimes(1);
  });

  it("finishes the Outbox entry without updating or closing an unmounted composer", async () => {
    const submission = deferred<string>();
    mocks.api.send.mockReturnValue(submission.promise);
    const view = render(<ComposeApp />);
    await screen.findByRole("button", { name: /^Send/ });
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "you@example.com" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    view.unmount();
    submission.resolve("queued");

    await waitFor(() =>
      expect(mocks.notifyOutbox).toHaveBeenCalledWith(
        expect.objectContaining({ phase: "finished" }),
      ),
    );
    expect(mocks.closeComposeWindow).not.toHaveBeenCalled();
  });

  it("reports a close failure after successful delivery without reopening send", async () => {
    vi.spyOn(window, "matchMedia").mockReturnValue({
      matches: true,
      media: "",
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    });
    mocks.api.send.mockResolvedValue("queued");
    mocks.closeComposeWindow.mockRejectedValue(
      new Error("window close blocked"),
    );
    await renderComposer();
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "you@example.com" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.closeComposeWindow).toHaveBeenCalledWith(true),
    );

    expect(mocks.closeComposeWindow).toHaveBeenCalledWith(true);
    expect(mocks.showNativeMessage).toHaveBeenCalledWith(
      "Message sent",
      "window close blocked",
      "error",
    );
    expect(screen.getByRole("button", { name: /Message sent/ })).toBeDisabled();
  });

  it("waits for the durable draft delete before closing after submission", async () => {
    vi.spyOn(window, "matchMedia").mockReturnValue({
      matches: true,
      media: "",
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    });
    const deleted = deferred<void>();
    mocks.api.send.mockResolvedValue("queued");
    mocks.discardComposeSeed.mockReturnValue(deleted.promise);
    await renderComposer();
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "you@example.com" },
    });

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));

    await waitFor(() =>
      expect(mocks.discardComposeSeed).toHaveBeenCalledOnce(),
    );
    expect(mocks.closeComposeWindow).not.toHaveBeenCalled();

    deleted.resolve();
    await waitFor(() =>
      expect(mocks.closeComposeWindow).toHaveBeenCalledWith(true),
    );
  });

  it("does not reject a delivered native draft while its child-local save is slow", async () => {
    vi.useFakeTimers();
    const committed = deferred<void>();
    let deliver:
      ((seed: { accountId: string; to: string }) => void) | undefined;
    mocks.hasNativeComposeSeedHandoff.mockReturnValue(true);
    mocks.onComposeTarget.mockImplementation(async (handler) => {
      deliver = handler;
      return () => undefined;
    });
    mocks.persistComposeSeed.mockReturnValue(committed.promise);

    render(<ComposeApp />);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(deliver).toEqual(expect.any(Function));
    act(() => deliver?.({ accountId: account.id, to: "cr@example.test" }));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(mocks.closeComposeWindow).not.toHaveBeenCalled();
    expect(mocks.showNativeMessage).not.toHaveBeenCalled();

    committed.resolve();
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByLabelText("To")).toHaveValue("cr@example.test");
  });

  it("closes a native child even when its handoff feedback cannot be displayed", async () => {
    mocks.hasNativeComposeSeedHandoff.mockReturnValue(true);
    mocks.onComposeTarget.mockRejectedValue(new Error("listener unavailable"));
    mocks.showNativeMessage.mockRejectedValue(new Error("dialog unavailable"));

    render(<ComposeApp />);

    await waitFor(() =>
      expect(mocks.closeComposeWindow).toHaveBeenCalledOnce(),
    );
  });
});
