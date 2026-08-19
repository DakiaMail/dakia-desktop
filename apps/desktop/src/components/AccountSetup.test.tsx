import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import "../i18n";
import type { Provider } from "../types";
import { AccountSetup } from "./AccountSetup";

const gmail: Provider = {
  id: "gmail",
  name: "Gmail",
  domains: ["gmail.com", "googlemail.com"],
  imap_host: "imap.gmail.com",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.gmail.com",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "[Gmail]/All Mail",
  spam_mailbox: "[Gmail]/Spam",
  oauth: true,
  app_password_help: "https://support.google.com/accounts/answer/185833",
};

const fastmail: Provider = {
  id: "fastmail",
  name: "Fastmail",
  domains: ["fastmail.com"],
  imap_host: "imap.fastmail.com",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.fastmail.com",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  oauth: false,
};

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

describe("AccountSetup provider guidance", () => {
  it("shows the temporary verification notice for detected Gmail accounts", () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
          onOAuth={vi.fn()}
        />
      </MantineProvider>,
    );

    const email = screen.getByRole("textbox", { name: "Email address" });
    fireEvent.change(email, { target: { value: "person@gmail.com" } });

    expect(screen.getByRole("status")).toHaveTextContent(
      "Dakia’s app is awaiting Google verification, so you may see an “unverified app” warning during sign-in. This should be resolved soon; use an app password in the meantime.",
    );
  });

  it("follows a manual provider override instead of the email domain", async () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
          onOAuth={vi.fn()}
        />
      </MantineProvider>,
    );

    const email = screen.getByRole("textbox", { name: "Email address" });
    fireEvent.change(email, { target: { value: "person@gmail.com" } });

    fireEvent.click(screen.getByRole("textbox", { name: "Provider" }));
    fireEvent.click(await screen.findByRole("option", { name: "Fastmail" }));

    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("shows the notice when Gmail is chosen for a custom-domain account", async () => {
    render(
      <MantineProvider>
        <AccountSetup
          providers={[gmail, fastmail]}
          saving={false}
          onSave={vi.fn()}
          onOAuth={vi.fn()}
        />
      </MantineProvider>,
    );

    const email = screen.getByRole("textbox", { name: "Email address" });
    fireEvent.change(email, { target: { value: "person@company.example" } });
    fireEvent.click(screen.getByRole("textbox", { name: "Provider" }));
    fireEvent.click(await screen.findByRole("option", { name: "Gmail" }));

    expect(screen.getByRole("status")).toBeVisible();
  });
});
