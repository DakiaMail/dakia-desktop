import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { Account } from "./types";

const legacyGoogleAccount: Account = {
  id: "account-google",
  email: "person@gmail.com",
  account_name: "Personal",
  display_name: "Person",
  provider_id: "gmail",
  auth: { type: "oauth2", username: "person@gmail.com", provider: "gmail" },
  imap_host: "imap.gmail.com",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.gmail.com",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "[Gmail]/All Mail",
  spam_mailbox: "[Gmail]/Spam",
  enabled: true,
};

const mocks = vi.hoisted(() => ({
  accounts: vi.fn(),
  realtimeSyncStatus: vi.fn(),
  mailRebuildStatus: vi.fn(),
  updateAccount: vi.fn(),
  notifyAccountUpdated: vi.fn(),
  showNativeMessage: vi.fn(),
}));

vi.mock("./api", () => ({
  api: {
    accounts: mocks.accounts,
    realtimeSyncStatus: mocks.realtimeSyncStatus,
    mailRebuildStatus: mocks.mailRebuildStatus,
    updateAccount: mocks.updateAccount,
  },
}));
vi.mock("./analytics", () => ({
  readAnalyticsSettings: () => ({ enabled: false }),
  listenForAnalyticsConsent: vi.fn(async () => () => undefined),
  setAnalyticsConsent: vi.fn((enabled: boolean) => ({ enabled })),
}));
vi.mock("@tauri-apps/plugin-autostart", () => ({
  disable: vi.fn(),
  enable: vi.fn(),
  isEnabled: vi.fn(async () => false),
}));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: vi.fn(),
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./notifications", () => ({
  notificationPermissionGranted: vi.fn(async () => false),
  readNotificationSettings: () => ({
    enabled: false,
    soundEnabled: false,
    showPreview: false,
  }),
  saveNotificationSettings: vi.fn(),
  sendTestNotification: vi.fn(),
}));
vi.mock("./nativeWindows", () => ({
  closeNativeWindow: vi.fn(),
  notifyAccountConnected: vi.fn(),
  notifyNotificationSettingsChanged: vi.fn(),
  notifySettingsChanged: vi.fn(),
  notifyAccountUpdated: mocks.notifyAccountUpdated,
  openAccountWindow: vi.fn(),
  onNativeMenuAction: vi.fn(async () => () => undefined),
  onAccountConnected: vi.fn(async () => () => undefined),
  onSettingsAccountSelected: vi.fn(async () => () => undefined),
  onMailSyncState: vi.fn(async () => () => undefined),
  onMailIndexRebuilt: vi.fn(async () => () => undefined),
  onMailRebuildFinished: vi.fn(async () => () => undefined),
  onMailRebuildProgress: vi.fn(async () => () => undefined),
}));
vi.mock("./components/Settings", () => ({
  Settings: ({
    accounts,
    accountSaving,
    onSaveAccount,
  }: {
    accounts: Account[];
    accountSaving: boolean;
    onSaveAccount: (input: Record<string, unknown>) => void;
  }) => (
    <div>
      <span data-testid="account-auth">{accounts[0]?.auth.type}</span>
      <span data-testid="account-saving">{String(accountSaving)}</span>
      <button
        type="button"
        onClick={() =>
          onSaveAccount({
            id: legacyGoogleAccount.id,
            password: "abcd efgh ijkl mnop",
          })
        }
      >
        Convert account
      </button>
    </div>
  ),
}));

describe("SettingsWindowApp legacy OAuth conversion", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.accounts.mockResolvedValue([legacyGoogleAccount]);
    mocks.realtimeSyncStatus.mockResolvedValue([]);
    mocks.mailRebuildStatus.mockResolvedValue([]);
  });

  it("optimistically converts auth and reloads authoritative OAuth state when persistence fails", async () => {
    let rejectUpdate!: (error: Error) => void;
    mocks.updateAccount.mockReturnValue(
      new Promise<Account>((_resolve, reject) => {
        rejectUpdate = reject;
      }),
    );
    const { SettingsWindowApp } = await import("./UtilityApps");
    render(
      <MantineProvider>
        <SettingsWindowApp />
      </MantineProvider>,
    );

    expect(await screen.findByTestId("account-auth")).toHaveTextContent(
      "oauth2",
    );
    fireEvent.click(screen.getByRole("button", { name: "Convert account" }));

    expect(screen.getByTestId("account-auth")).toHaveTextContent("password");
    expect(screen.getByTestId("account-saving")).toHaveTextContent("true");
    expect(mocks.updateAccount).toHaveBeenCalledWith({
      id: legacyGoogleAccount.id,
      password: "abcd efgh ijkl mnop",
    });

    rejectUpdate(new Error("database is locked"));

    await waitFor(() =>
      expect(screen.getByTestId("account-auth")).toHaveTextContent("oauth2"),
    );
    expect(mocks.accounts).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("account-saving")).toHaveTextContent("false");
    expect(mocks.notifyAccountUpdated).not.toHaveBeenCalled();
    expect(mocks.showNativeMessage).toHaveBeenCalledWith(
      "Could not save account",
      "database is locked",
      "error",
    );
  });

  it("keeps the authoritative password state when only post-save reconciliation fails", async () => {
    const convertedAccount: Account = {
      ...legacyGoogleAccount,
      auth: { type: "password", username: "person@gmail.com" },
    };
    mocks.accounts
      .mockResolvedValueOnce([legacyGoogleAccount])
      .mockResolvedValueOnce([convertedAccount]);
    mocks.updateAccount.mockRejectedValueOnce(
      new Error("realtime reconciliation failed"),
    );
    const { SettingsWindowApp } = await import("./UtilityApps");
    render(
      <MantineProvider>
        <SettingsWindowApp />
      </MantineProvider>,
    );

    expect(await screen.findByTestId("account-auth")).toHaveTextContent(
      "oauth2",
    );
    fireEvent.click(screen.getByRole("button", { name: "Convert account" }));

    expect(screen.getByTestId("account-auth")).toHaveTextContent("password");
    await waitFor(() => expect(mocks.accounts).toHaveBeenCalledTimes(2));
    expect(screen.getByTestId("account-auth")).toHaveTextContent("password");
    expect(mocks.notifyAccountUpdated).not.toHaveBeenCalled();
    expect(mocks.showNativeMessage).toHaveBeenCalledWith(
      "Could not save account",
      "realtime reconciliation failed",
      "error",
    );
  });
});
