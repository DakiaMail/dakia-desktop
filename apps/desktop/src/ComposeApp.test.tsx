import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { Account } from "./types";

const mocks = vi.hoisted(() => ({
  accounts: vi.fn(),
  forwardAttachments: vi.fn(),
  aiAvailable: vi.fn(),
  readComposeSeed: vi.fn(),
  readDatabaseComposeSeed: vi.fn(),
}));

vi.mock("./api", () => ({
  api: {
    accounts: mocks.accounts,
    forwardAttachments: mocks.forwardAttachments,
    aiAvailable: mocks.aiAvailable,
  },
}));

vi.mock("./composeWindow", () => ({
  readComposeSeed: mocks.readComposeSeed,
  readDatabaseComposeSeed: mocks.readDatabaseComposeSeed,
  closeComposeWindow: vi.fn(),
  notifyOutbox: vi.fn(),
}));

vi.mock("./nativeFeedback", () => ({ showNativeMessage: vi.fn() }));

vi.mock("./components/Composer", () => ({
  Composer: ({ accounts }: { accounts: Account[] }) => (
    <output data-testid="composer-accounts">
      {accounts.map((account) => account.id).join(",")}
    </output>
  ),
}));

import { ComposeApp } from "./ComposeApp";

const enabled: Account = {
  id: "enabled",
  email: "enabled@example.com",
  account_name: "enabled@example.com",
  display_name: "Enabled",
  provider_id: "test",
  auth: { type: "password", username: "enabled@example.com" },
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

const disabled: Account = {
  ...enabled,
  id: "disabled",
  email: "disabled@example.com",
  enabled: false,
};

describe("ComposeApp account eligibility", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.readComposeSeed.mockReturnValue({});
    mocks.readDatabaseComposeSeed.mockResolvedValue(undefined);
    mocks.forwardAttachments.mockResolvedValue([]);
    mocks.aiAvailable.mockResolvedValue(false);
  });

  it("passes only enabled accounts to Composer", async () => {
    mocks.accounts.mockResolvedValue([disabled, enabled]);
    render(<ComposeApp />);

    expect(await screen.findByTestId("composer-accounts")).toHaveTextContent(
      "enabled",
    );
  });

  it("shows the existing no-account state when every account is disabled", async () => {
    mocks.accounts.mockResolvedValue([disabled]);
    render(<ComposeApp />);

    await waitFor(() =>
      expect(
        screen.getByText("Connect an account before composing."),
      ).toBeVisible(),
    );
    expect(screen.queryByTestId("composer-accounts")).toBeNull();
  });
});
