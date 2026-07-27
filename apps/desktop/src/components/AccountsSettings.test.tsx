import { MantineProvider } from "@mantine/core";
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "../i18n";
import type { Account } from "../types";
import { AccountsSettings } from "./AccountsSettings";

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
