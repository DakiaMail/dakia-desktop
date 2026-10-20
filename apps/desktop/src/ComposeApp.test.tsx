import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { ComposeApp } from "./ComposeApp";
import type { Account, SendSubmission } from "./types";

const mocks = vi.hoisted(() => ({
  accounts: vi.fn(),
  forwardAttachments: vi.fn(),
  aiAvailable: vi.fn(),
  draft: vi.fn(),
  sendOutcome: vi.fn(),
  notifyOutbox: vi.fn(),
  closeComposeWindow: vi.fn(),
}));

vi.mock("./api", () => ({
  api: {
    accounts: mocks.accounts,
    forwardAttachments: mocks.forwardAttachments,
    aiAvailable: mocks.aiAvailable,
    draft: mocks.draft,
    sendOutcome: mocks.sendOutcome,
  },
}));

vi.mock("./composeWindow", () => ({
  closeComposeWindow: mocks.closeComposeWindow,
  notifyOutbox: mocks.notifyOutbox,
  readComposeSeed: () => ({}),
  readDatabaseComposeSeed: async () => undefined,
}));

vi.mock("./nativeFeedback", () => ({ showNativeMessage: vi.fn() }));

vi.mock("./components/Composer", () => ({
  Composer: ({
    sendState,
    onSend,
  }: {
    sendState: string;
    onSend: (draft: Record<string, unknown>) => void;
  }) => (
    <button
      type="button"
      onClick={() =>
        onSend({
          account_id: "account-1",
          to: ["recipient@example.test"],
          subject: "Outcome contract",
        })
      }
    >
      {sendState}
    </button>
  ),
}));

const account: Account = {
  id: "account-1",
  email: "me@example.test",
  account_name: "Me",
  display_name: "Me",
  provider_id: "test",
  auth: { type: "password", username: "me@example.test" },
  imap_host: "imap.example.test",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.example.test",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};

const accepted: SendSubmission = {
  operationId: "operation-1",
  status: "accepted",
  response: "250 queued",
};

beforeEach(() => {
  mocks.accounts.mockResolvedValue([account]);
  mocks.forwardAttachments.mockResolvedValue([]);
  mocks.aiAvailable.mockResolvedValue(false);
  mocks.draft.mockResolvedValue("");
  mocks.notifyOutbox.mockResolvedValue(undefined);
  mocks.closeComposeWindow.mockResolvedValue(undefined);
  mocks.sendOutcome.mockResolvedValue(accepted);
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("ComposeApp submission outcomes", () => {
  it("closes after SMTP acceptance while the Sent copy is saved in the background", async () => {
    const pending: SendSubmission = {
      operationId: "operation-2",
      status: "sent_copy_pending",
      response: "250 queued",
    };
    mocks.sendOutcome.mockResolvedValue(pending);
    render(<ComposeApp />);

    fireEvent.click(await screen.findByRole("button", { name: "idle" }));
    expect(
      await screen.findByRole("button", { name: "sent_copy_pending" }),
    ).toBeVisible();
    await waitFor(() => expect(mocks.closeComposeWindow).toHaveBeenCalled());

    expect(mocks.notifyOutbox).toHaveBeenNthCalledWith(
      2,
      expect.objectContaining({ phase: "finished" }),
    );
    expect(mocks.closeComposeWindow).toHaveBeenCalledWith(pending);
  });

  it("keeps the composer disabled after an uncertain SMTP result and never closes it", async () => {
    mocks.sendOutcome.mockResolvedValue({
      operationId: "operation-3",
      status: "uncertain",
    } satisfies SendSubmission);
    render(<ComposeApp />);

    fireEvent.click(await screen.findByRole("button", { name: "idle" }));
    expect(
      await screen.findByRole("button", { name: "uncertain" }),
    ).toBeVisible();
    expect(mocks.notifyOutbox).toHaveBeenNthCalledWith(
      2,
      expect.objectContaining({ phase: "finished" }),
    );
    expect(mocks.closeComposeWindow).not.toHaveBeenCalled();
  });

  it("closes after accepted SMTP when only local persistence needs reconciliation", async () => {
    const pending: SendSubmission = {
      operationId: "operation-4",
      status: "accepted",
      persistenceWarning: true,
    };
    mocks.sendOutcome.mockResolvedValue(pending);
    render(<ComposeApp />);

    fireEvent.click(await screen.findByRole("button", { name: "idle" }));
    expect(
      await screen.findByRole("button", {
        name: "sent_persistence_pending",
      }),
    ).toBeVisible();
    await waitFor(() =>
      expect(mocks.closeComposeWindow).toHaveBeenCalledWith(pending),
    );
  });

  it("keeps a durable queued submission non-resendable while it closes into background delivery", async () => {
    const queued: SendSubmission = {
      operationId: "operation-5",
      status: "queued",
    };
    mocks.sendOutcome.mockResolvedValue(queued);
    render(<ComposeApp />);

    fireEvent.click(await screen.findByRole("button", { name: "idle" }));
    expect(await screen.findByRole("button", { name: "queued" })).toBeVisible();
    await waitFor(() =>
      expect(mocks.closeComposeWindow).toHaveBeenCalledWith(queued),
    );
  });
});
