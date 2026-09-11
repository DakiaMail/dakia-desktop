import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import type { Account } from "../types";
import { AccountsSettings } from "./AccountsSettings";

const mocks = vi.hoisted(() => ({
  openExternal: vi.fn(),
}));

vi.mock("../api", () => ({
  api: { openExternal: mocks.openExternal },
}));

const account: Account = {
  id: "account-1",
  email: "person@example.com",
  account_name: "Personal",
  display_name: "Person",
  provider_id: "fastmail",
  auth: { type: "password", username: "person@example.com" },
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

const legacyGoogleAccount: Account = {
  ...account,
  id: "account-google",
  email: "person@gmail.com",
  provider_id: "gmail",
  auth: { type: "oauth2", username: "person@gmail.com", provider: "gmail" },
};

beforeEach(() => {
  vi.clearAllMocks();
});

describe("AccountsSettings rebuild progress", () => {
  it("shows determinate catalogue progress during a full rebuild", () => {
    render(
      <MantineProvider>
        <AccountsSettings
          accounts={[account]}
          saving={false}
          removing={false}
          fullSyncing
          fullSyncProgress={{
            phase: "downloading",
            completed: 50,
            total: 100,
          }}
          realtimeStatuses={[]}
          onAdd={vi.fn()}
          onSave={vi.fn()}
          onRemove={vi.fn()}
          onFullSync={vi.fn()}
        />
      </MantineProvider>,
    );

    expect(screen.getByText("Indexing message 50 of 100…")).toBeVisible();
    expect(
      screen.getByRole("progressbar", {
        name: "Indexing message 50 of 100…",
      }),
    ).toHaveAttribute("aria-valuenow", "50");
  });
});

describe("AccountsSettings legacy Google OAuth recovery", () => {
  const baseProps = {
    accounts: [legacyGoogleAccount],
    saving: false,
    removing: false,
    fullSyncing: false,
    onAdd: vi.fn(),
    onSave: vi.fn(),
    onRemove: vi.fn(),
    onFullSync: vi.fn(),
  };

  it("shows no migration guidance for healthy OAuth or connection failures", () => {
    const { rerender } = render(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "idle",
              errorKind: null,
            },
          ]}
        />
      </MantineProvider>,
    );

    expect(
      screen.queryByText("Gmail sign-in needs an app password"),
    ).not.toBeInTheDocument();

    rerender(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "paused",
              errorKind: "connection",
            },
          ]}
        />
      </MantineProvider>,
    );
    expect(
      screen.queryByText("Gmail sign-in needs an app password"),
    ).not.toBeInTheDocument();

    rerender(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          accounts={[{ ...legacyGoogleAccount, auth: account.auth }]}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "paused",
              errorKind: "authentication",
            },
          ]}
        />
      </MantineProvider>,
    );
    expect(
      screen.queryByText("Gmail sign-in needs an app password"),
    ).not.toBeInTheDocument();
  });

  it("does not offer the Gmail migration for another provider's OAuth account", () => {
    render(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          accounts={[
            {
              ...legacyGoogleAccount,
              email: "person@example.test",
              provider_id: "outlook",
              auth: {
                type: "oauth2",
                username: "person@example.test",
                provider: "outlook",
              },
            },
          ]}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "paused",
              errorKind: "authentication",
            },
          ]}
        />
      </MantineProvider>,
    );

    expect(
      screen.queryByText("Gmail sign-in needs an app password"),
    ).not.toBeInTheDocument();
  });

  it("offers an app-password conversion only for a paused authentication failure", () => {
    const onSave = vi.fn();
    render(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          onSave={onSave}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "paused",
              errorKind: "authentication",
            },
          ]}
        />
      </MantineProvider>,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Gmail sign-in needs an app password",
    );
    expect(
      screen.getByLabelText("Google app password"),
    ).toHaveAccessibleDescription(
      "Do not use your personal Gmail or regular Google Account password in Dakia.",
    );
    fireEvent.click(
      screen.getByRole("button", {
        name: "Open Google’s app-password guide",
      }),
    );
    expect(mocks.openExternal).toHaveBeenCalledWith(
      "https://support.google.com/accounts/answer/185833?hl=en",
    );
    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "abcd efgh ijkl mnop" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({
        id: legacyGoogleAccount.id,
        password: "abcd efgh ijkl mnop",
      }),
    );
  });

  it("requires a non-whitespace app password before conversion can be saved", () => {
    render(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          realtimeStatuses={[
            {
              accountId: legacyGoogleAccount.id,
              state: "paused",
              errorKind: "authentication",
            },
          ]}
        />
      </MantineProvider>,
    );

    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toBeDisabled();

    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "   " },
    });
    expect(save).toBeDisabled();

    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "abcd efgh ijkl mnop" },
    });
    expect(save).toBeEnabled();
  });

  it("clears the submitted credential after successful conversion", () => {
    const realtimeStatuses = [
      {
        accountId: legacyGoogleAccount.id,
        state: "paused" as const,
        errorKind: "authentication" as const,
      },
    ];
    const { rerender } = render(
      <MantineProvider>
        <AccountsSettings {...baseProps} realtimeStatuses={realtimeStatuses} />
      </MantineProvider>,
    );
    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "abcd efgh ijkl mnop" },
    });

    rerender(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          accounts={[{ ...legacyGoogleAccount, auth: account.auth }]}
          realtimeStatuses={realtimeStatuses}
        />
      </MantineProvider>,
    );

    expect(
      screen.queryByDisplayValue("abcd efgh ijkl mnop"),
    ).not.toBeInTheDocument();
    expect(screen.getByLabelText("New password or app password")).toHaveValue(
      "",
    );
  });

  it("clears the submitted credential after a failed conversion rolls back", () => {
    const realtimeStatuses = [
      {
        accountId: legacyGoogleAccount.id,
        state: "paused" as const,
        errorKind: "authentication" as const,
      },
    ];
    const { rerender } = render(
      <MantineProvider>
        <AccountsSettings {...baseProps} realtimeStatuses={realtimeStatuses} />
      </MantineProvider>,
    );
    fireEvent.change(screen.getByLabelText("Google app password"), {
      target: { value: "abcd efgh ijkl mnop" },
    });

    rerender(
      <MantineProvider>
        <AccountsSettings
          {...baseProps}
          accounts={[{ ...legacyGoogleAccount, auth: account.auth }]}
          realtimeStatuses={realtimeStatuses}
        />
      </MantineProvider>,
    );
    rerender(
      <MantineProvider>
        <AccountsSettings {...baseProps} realtimeStatuses={realtimeStatuses} />
      </MantineProvider>,
    );

    expect(
      screen.queryByDisplayValue("abcd efgh ijkl mnop"),
    ).not.toBeInTheDocument();
    expect(screen.getByLabelText("Google app password")).toHaveValue("");
  });
});
