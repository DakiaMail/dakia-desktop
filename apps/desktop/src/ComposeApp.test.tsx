import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { ComposeApp } from "./ComposeApp";

const mocks = vi.hoisted(() => ({
  api: {
    accounts: vi.fn(),
    aiAvailable: vi.fn(),
    draft: vi.fn(),
    forwardAttachments: vi.fn(),
    inspectComposeImage: vi.fn(),
    reduceComposeImage: vi.fn(),
    send: vi.fn(),
  },
  closeComposeWindow: vi.fn(),
  confirmNativeAction: vi.fn(),
  notifyOutbox: vi.fn(),
  readComposeSeed: vi.fn(),
  readDatabaseComposeSeed: vi.fn(),
  showNativeMessage: vi.fn(),
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./composeWindow", () => ({
  closeComposeWindow: mocks.closeComposeWindow,
  notifyOutbox: mocks.notifyOutbox,
  readComposeSeed: mocks.readComposeSeed,
  readDatabaseComposeSeed: mocks.readDatabaseComposeSeed,
}));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: mocks.confirmNativeAction,
  showNativeMessage: mocks.showNativeMessage,
}));

const originalAttachment = {
  filename: "camera.png",
  mime_type: "image/png",
  content_base64: "original-bytes",
  size_bytes: 2 * 1024 * 1024 + 1,
};

const reducedAttachment = {
  filename: "camera.jpg",
  mime_type: "image/jpeg",
  content_base64: "reduced-bytes",
  size_bytes: 420_000,
};

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

describe("ComposeApp image reduction send boundary", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.readComposeSeed.mockReturnValue({
      to: "you@example.com",
      attachments: [originalAttachment],
    });
    mocks.readDatabaseComposeSeed.mockResolvedValue(null);
    mocks.api.accounts.mockResolvedValue([
      {
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
      },
    ]);
    mocks.api.inspectComposeImage.mockResolvedValue({
      eligible: true,
      reason: null,
    });
    mocks.confirmNativeAction.mockResolvedValue(true);
    mocks.api.send.mockResolvedValue(undefined);
    mocks.notifyOutbox.mockResolvedValue(undefined);
    mocks.closeComposeWindow.mockResolvedValue(undefined);
    vi.stubGlobal(
      "matchMedia",
      vi.fn(() => ({ matches: true })),
    );
  });

  it("starts the optimistic Outbox and SMTP request once with final bytes", async () => {
    const reduction = deferred<{
      status: "reduced";
      attachment: typeof reducedAttachment;
      original_size_bytes: number;
      reduced_size_bytes: number;
      reason: null;
    }>();
    mocks.api.reduceComposeImage.mockReturnValue(reduction.promise);

    render(<ComposeApp />);

    const send = await screen.findByRole("button", { name: /send/i });
    await waitFor(() =>
      expect(mocks.api.reduceComposeImage).toHaveBeenCalled(),
    );
    expect(send).toBeDisabled();
    expect(mocks.notifyOutbox).not.toHaveBeenCalled();
    expect(mocks.api.send).not.toHaveBeenCalled();

    await act(async () => {
      reduction.resolve({
        status: "reduced",
        attachment: reducedAttachment,
        original_size_bytes: originalAttachment.size_bytes,
        reduced_size_bytes: reducedAttachment.size_bytes,
        reason: null,
      });
      await reduction.promise;
    });
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.click(send);

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledTimes(1));
    expect(mocks.notifyOutbox).toHaveBeenCalledTimes(2);
    expect(mocks.notifyOutbox.mock.calls[0]?.[0]).toMatchObject({
      phase: "sending",
      message: { has_attachments: true },
    });
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        attachments: [
          {
            filename: "camera.jpg",
            mime_type: "image/jpeg",
            content_base64: "reduced-bytes",
          },
        ],
      }),
    );
    expect(mocks.notifyOutbox.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.api.send.mock.invocationCallOrder[0],
    );
  });
});
